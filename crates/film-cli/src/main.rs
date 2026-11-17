//! M0 垂直切片：合成负片 → 片基估计 → 解耦 → 虚拟印相 → 指标验收。

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use film_core::image::{FloatImage, Linearize};
use film_core::metrics::{
    crop_margin, mean_delta_e00, mean_delta_e00_on_reference_neutrals, neutral_axis_drift,
    neutral_axis_shift, psnr,
};
use film_core::pipeline::{process, ProcessParams};
use film_core::print::PrintParams;
use film_core::synth::{synthetic_negative, SynthOptions, BASE_TRANSMITTANCE};
use film_core::{calib, NegativeModel};

#[derive(Parser)]
#[command(name = "film", about = "C-41 负片去色罩（M0 垂直切片）")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 生成合成负片（含片基边框）与参考正片
    Synth {
        #[arg(long, default_value = "neg.tif")]
        out: PathBuf,
        #[arg(long, default_value = "ref.png")]
        reference: PathBuf,
        #[arg(long, default_value_t = 900)]
        width: usize,
        #[arg(long, default_value_t = 600)]
        height: usize,
        #[arg(long, default_value_t = 24)]
        rebate: usize,
        #[arg(long, default_value_t = 0.0)]
        noise: f32,
    },
    /// 负片 → 正片
    Invert {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// 输入编码：1 = 线性，2.2 / 1.8 = 伽马，-1 = sRGB
        #[arg(long, default_value_t = 1.0)]
        gamma: f32,
        /// 手动片基（"r,g,b"），缺省自动检测
        #[arg(long)]
        base: Option<String>,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        fit_unmix: bool,
        #[arg(long, default_value_t = 0.02)]
        lambda_prior: f32,
        #[arg(long, default_value_t = 1.0)]
        grade: f32,
        #[arg(long, default_value_t = 0.0)]
        exposure: f32,
        #[arg(long, default_value_t = 0.0)]
        saturation: f32,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        auto_exposure: bool,
        #[arg(long, default_value_t = 0.45)]
        target: f32,
    },
    /// 只做标定诊断（不写图）
    Analyze {
        #[arg(long)]
        input: PathBuf,
        #[arg(long, default_value_t = 1.0)]
        gamma: f32,
    },
    /// 自检：合成 → 反演 → 与参考正片比对
    Selftest {
        #[arg(long, default_value_t = 900)]
        width: usize,
        #[arg(long, default_value_t = 600)]
        height: usize,
        #[arg(long, default_value_t = 0.0)]
        noise: f32,
    },
}

fn linearize(gamma: f32) -> (Linearize, f32) {
    if (gamma - 1.0).abs() < 1e-3 {
        (Linearize::None, 1.0)
    } else if gamma < 0.0 {
        (Linearize::Srgb, 2.2)
    } else {
        (Linearize::Gamma, gamma)
    }
}

fn parse_base(s: &str) -> anyhow::Result<[f32; 3]> {
    let parts: Vec<f32> = s
        .split(',')
        .map(|p| p.trim().parse::<f32>())
        .collect::<Result<_, _>>()?;
    anyhow::ensure!(parts.len() == 3, "片基需要三个通道值，如 0.30,0.18,0.09");
    Ok([parts[0], parts[1], parts[2]])
}

fn save_any(img: &FloatImage, path: &Path) -> anyhow::Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "tif" || ext == "tiff" {
        img.save_tiff16(path)?;
    } else {
        img.save_display8(path)?;
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Synth {
            out,
            reference,
            width,
            height,
            rebate,
            noise,
        } => {
            let opts = SynthOptions {
                width,
                height,
                rebate,
                noise,
                ..Default::default()
            };
            let (neg, pos, _truth) = synthetic_negative(&opts);
            save_any(&neg, &out)?;
            save_any(&pos, &reference)?;
            println!("负片: {}", out.display());
            println!("参考正片: {}", reference.display());
        }

        Cmd::Invert {
            input,
            out,
            gamma,
            base,
            fit_unmix,
            lambda_prior,
            grade,
            exposure,
            saturation,
            auto_exposure,
            target,
        } => {
            let (lin, g) = linearize(gamma);
            let img = FloatImage::load(&input, lin, g)?;
            let mut params = ProcessParams {
                fit_unmix,
                lambda_prior,
                auto_exposure,
                exposure_target: target,
                base: match base.as_deref() {
                    Some(s) => Some(parse_base(s)?),
                    None => None,
                },
                print: PrintParams {
                    grade,
                    exposure,
                    saturation,
                    ..Default::default()
                },
                ..Default::default()
            };
            params.print.d_ref = PrintParams::default().d_ref;

            let t0 = std::time::Instant::now();
            let out_res = process(&img, &params);
            let dt = t0.elapsed();

            save_any(&out_res.image, &out)?;
            println!(
                "片基: [{:.4}, {:.4}, {:.4}]  置信度 {:.2} ({:?})",
                out_res.base.base[0],
                out_res.base.base[1],
                out_res.base.base[2],
                out_res.base.confidence,
                out_res.base.source
            );
            println!(
                "逐段增益: {:?}  (样本 {}, 残余色度 {:.4}, 置信度 {:.2})",
                out_res.fit.gain.g,
                out_res.fit.samples,
                out_res.fit.residual,
                out_res.fit.confidence
            );
            println!(
                "中位染料密度: [{:.3}, {:.3}, {:.3}]  印相曝光偏置 {:.3}",
                out_res.median_dye[0], out_res.median_dye[1], out_res.median_dye[2], out_res.exposure
            );
            println!("输出: {}  ({:.1}ms, {}×{})", out.display(), dt.as_secs_f32() * 1000.0, out_res.image.width, out_res.image.height);
            println!(
                "d_ref {:?}  anchor lo {:?} med {:?} hi {:?} t {:?}",
                out_res.print.d_ref, out_res.print.anchor_lo, out_res.print.anchor_med,
                out_res.print.anchor_hi, out_res.print.anchor_t
            );
        }

        Cmd::Analyze { input, gamma } => {
            let (lin, g) = linearize(gamma);
            let img = FloatImage::load(&input, lin, g)?;
            let est = calib::estimate_base(&img);
            println!(
                "片基: [{:.4}, {:.4}, {:.4}]  置信度 {:.2} 来源 {:?}",
                est.base[0], est.base[1], est.base[2], est.confidence, est.source
            );
            let model0 = NegativeModel::from_matrix(
                est.base,
                &film_core::model::default_matrix(),
                [2.1, 2.2, 2.1],
            )
            .ok_or_else(|| anyhow::anyhow!("先验矩阵不可逆"))?;
            let fit = calib::fit_gains(&img, &model0, 0.05);
            println!("逐段增益: {:?}", fit.gain.g);
            println!("先验解耦 B: {:?}", model0.unmix);
            println!(
                "中性样本 {} 个，修正后残余色度 {:.4}，置信度 {:.2}",
                fit.samples, fit.residual, fit.confidence
            );
            println!("相纸锚点估计: {:?}", calib::estimate_d_ref(&img, &model0));
            println!(
                "尺寸 {}×{}，采样 {} px",
                img.width,
                img.height,
                img.len()
            );
        }

        Cmd::Selftest { width, height, noise } => {
            run_selftest(width, height, noise)?;
        }
    }
    Ok(())
}

fn run_selftest(width: usize, height: usize, noise: f32) -> anyhow::Result<()> {
    println!("== M0 自检：合成负片 → 反演 → 与参考正片比对 ==");
    let opts = SynthOptions {
        width,
        height,
        rebate: 24,
        noise,
        ..Default::default()
    };
    let (neg, reference, truth) = synthetic_negative(&opts);

    // 1) 片基
    let est = calib::estimate_base(&neg);
    println!(
        "片基估计: [{:.4}, {:.4}, {:.4}]  真值 [{:.4}, {:.4}, {:.4}]  置信度 {:.2} ({:?})",
        est.base[0], est.base[1], est.base[2],
        BASE_TRANSMITTANCE[0], BASE_TRANSMITTANCE[1], BASE_TRANSMITTANCE[2],
        est.confidence, est.source
    );

    // 2) 逐段中性增益
    let model0 = NegativeModel::from_matrix(est.base, &opts.a, PrintParams::default().d_ref)
        .expect("invertible prior");
    let fit = calib::fit_gains(&neg, &model0, 0.05);
    println!(
        "逐段增益 = {:?}\n  样本 {}, 修正后残余色度 {:.4}, 置信度 {:.2}",
        fit.gain.g, fit.samples, fit.residual, fit.confidence
    );

    // 3) 染料密度还原误差（真值来自合成器）
    let model = NegativeModel {
        base: est.base,
        unmix: model0.unmix,
        d_ref: PrintParams::default().d_ref,
        gain: fit.gain.clone(),
    };
    let mut max_err = 0.0f32;
    let mut mean_err = 0.0f64;
    let mut cnt = 0f64;
    for y in (opts.rebate + 2)..(height - opts.rebate - 2) {
        for x in (opts.rebate + 2)..(width - opts.rebate - 2) {
            let i = y * width + x;
            let d = model.dye_at(neg.pixel(i));
            for c in 0..3 {
                let e = (d[c] - truth[i][c]).abs();
                max_err = max_err.max(e);
                mean_err += e as f64;
                cnt += 1.0;
            }
        }
    }
    println!(
        "染料密度还原: 最大误差 {:.4}，平均 {:.4}（单位：密度）",
        max_err,
        mean_err / cnt.max(1.0)
    );

    // 4) 印相输出 vs 参考
    let pp = PrintParams::default();
    let mut out = FloatImage::new(width, height);
    for i in 0..neg.len() {
        out.set_pixel(i, pp.apply(model.dye_at(neg.pixel(i))));
    }
    // 指标只看画面区（片基边框不属于成像内容）
    let m = opts.rebate + 2;
    let out_c = crop_margin(&out, m);
    let ref_c = crop_margin(&reference, m);
    let p = psnr(&out_c, &ref_c);
    let de = mean_delta_e00(&out_c, &ref_c);
    let (de_neutral, n_neutral) = mean_delta_e00_on_reference_neutrals(&out_c, &ref_c, 5.0);
    println!("PSNR = {:.2} dB（画面区）, 平均 ΔE00 = {:.2}", p, de);
    println!(
        "参考中性像素上 ΔE00 = {:.2}（{} 个样本）",
        de_neutral, n_neutral
    );
    println!(
        "中性轴相对参考的漂移（8 段，Lab 单位）: {:?}",
        neutral_axis_shift(&out_c, &ref_c)
    );
    println!("中性轴绝对漂移（按 L 分 8 段）: {:?}", neutral_axis_drift(&out_c));

    // 容差随噪声放宽：2% 全幅噪声 ≈ 0.0087 密度，误差 1.5 倍以内算正常
    let tol = 0.004 + 1.5 * (opts.noise as f32 / 2.3026);
    anyhow::ensure!(max_err < tol, "染料还原误差过大: {max_err} > {tol}");
    anyhow::ensure!(de < 3.0, "ΔE00 过大: {de}");
    println!("自检通过。");
    Ok(())
}
