//! 客观评价：PSNR、ΔE00（近似）、中性轴漂移。用指标验收，而不是"看着像"。

use crate::image::FloatImage;

pub fn psnr(a: &FloatImage, b: &FloatImage) -> f32 {
    assert_eq!(a.width, b.width);
    assert_eq!(a.height, b.height);
    let mut se = 0.0f64;
    let n = a.len() * 3;
    for i in 0..a.len() {
        let pa = a.pixel(i);
        let pb = b.pixel(i);
        for c in 0..3 {
            let d = (pa[c] - pb[c]) as f64;
            se += d * d;
        }
    }
    let mse = se / n as f64;
    if mse <= 1e-12 {
        return 99.0;
    }
    (10.0 * (1.0 / mse).log10()) as f32
}

/// 线性 sRGB → CIELAB（D65）。
pub fn linear_to_lab(c: [f32; 3]) -> [f32; 3] {
    let m = [
        [0.4124f32, 0.3576, 0.1805],
        [0.2126, 0.7152, 0.0722],
        [0.0193, 0.1192, 0.9505],
    ];
    let xyz = [
        m[0][0] * c[0] + m[0][1] * c[1] + m[0][2] * c[2],
        m[1][0] * c[0] + m[1][1] * c[1] + m[1][2] * c[2],
        m[2][0] * c[0] + m[2][1] * c[1] + m[2][2] * c[2],
    ];
    let white = [0.9505f32, 1.0, 1.089];
    let f = |t: f32| -> f32 {
        let t = t.max(1e-6);
        if t > 0.008856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let fx = f(xyz[0] / white[0]);
    let fy = f(xyz[1] / white[1]);
    let fz = f(xyz[2] / white[2]);
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// ΔE00（CIEDE2000），逐像素均值。
pub fn mean_delta_e00(a: &FloatImage, b: &FloatImage) -> f32 {
    let mut sum = 0.0f64;
    let mut cnt = 0;
    let stride = (a.len() / 50_000).max(1);
    for i in (0..a.len()).step_by(stride) {
        let la = linear_to_lab(a.pixel(i));
        let lb = linear_to_lab(b.pixel(i));
        sum += delta_e00(la, lb) as f64;
        cnt += 1;
    }
    (sum / cnt.max(1) as f64) as f32
}

pub fn delta_e00(lab1: [f32; 3], lab2: [f32; 3]) -> f32 {
    let (l1, a1, b1) = (lab1[0], lab1[1], lab1[2]);
    let (l2, a2, b2) = (lab2[0], lab2[1], lab2[2]);
    let c1 = (a1 * a1 + b1 * b1).sqrt();
    let c2 = (a2 * a2 + b2 * b2).sqrt();
    let cbar = (c1 + c2) / 2.0;
    let g = 0.5 * (1.0 - (cbar.powi(7) / (cbar.powi(7) + 25f32.powi(7))).sqrt());
    let a1p = (1.0 + g) * a1;
    let a2p = (1.0 + g) * a2;
    let c1p = (a1p * a1p + b1 * b1).sqrt();
    let c2p = (a2p * a2p + b2 * b2).sqrt();
    let h1p = if b1 == 0.0 && a1p == 0.0 {
        0.0
    } else {
        b1.atan2(a1p).to_degrees().rem_euclid(360.0)
    };
    let h2p = if b2 == 0.0 && a2p == 0.0 {
        0.0
    } else {
        b2.atan2(a2p).to_degrees().rem_euclid(360.0)
    };
    let dlp = l2 - l1;
    let dcp = c2p - c1p;
    let dhp = if c1p * c2p < 1e-9 {
        0.0
    } else if (h2p - h1p).abs() <= 180.0 {
        h2p - h1p
    } else if h2p - h1p > 180.0 {
        h2p - h1p - 360.0
    } else {
        h2p - h1p + 360.0
    };
    let dhp_big = 2.0 * (c1p * c2p).sqrt() * (dhp.to_radians() / 2.0).sin();
    let lbar = (l1 + l2) / 2.0;
    let cbar_p = (c1p + c2p) / 2.0;
    let hbar_p = if c1p * c2p < 1e-9 {
        h1p + h2p
    } else if (h1p - h2p).abs() <= 180.0 {
        (h1p + h2p) / 2.0
    } else if h1p + h2p < 360.0 {
        (h1p + h2p + 360.0) / 2.0
    } else {
        (h1p + h2p - 360.0) / 2.0
    };
    let t = 1.0 - 0.17 * (hbar_p - 30.0).to_radians().cos()
        + 0.24 * (2.0 * hbar_p).to_radians().cos()
        + 0.32 * (3.0 * hbar_p + 6.0).to_radians().cos()
        - 0.20 * (4.0 * hbar_p - 63.0).to_radians().cos();
    let sl = 1.0 + (0.015 * (lbar - 50.0).powi(2)) / (20.0 + (lbar - 50.0).powi(2)).sqrt();
    let sc = 1.0 + 0.045 * cbar_p;
    let sh = 1.0 + 0.015 * cbar_p * t;
    let rt = -2.0 * (cbar_p.powi(7) / (cbar_p.powi(7) + 25f32.powi(7))).sqrt()
        * (60.0 * (-((hbar_p - 275.0) / 25.0).powi(2)).exp()).to_radians().sin();
    let tl = dlp / sl;
    let tc = dcp / sc;
    let th = dhp_big / sh;
    (tl * tl + tc * tc + th * th + rt * tc * th).max(0.0).sqrt()
}

/// 去掉四周边框后的画面区（边框在图内时指标才有意义）。
pub fn crop_margin(img: &FloatImage, margin: usize) -> FloatImage {
    let m = margin.min(img.width / 2).min(img.height / 2);
    let w = img.width.saturating_sub(2 * m).max(1);
    let h = img.height.saturating_sub(2 * m).max(1);
    let mut out = FloatImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            out.set_pixel(y * w + x, img.pixel((y + m) * img.width + x + m));
        }
    }
    out
}

/// 中性轴漂移：按亮度分 8 段，每段取**彩度最低的 10%** 像素，统计其平均 (a,b) 模长。
/// 0 = 完美中性；真实照片里整幅平均彩度不说明问题，必须选择性统计近中性像素。
pub fn neutral_axis_drift(img: &FloatImage) -> [f32; 8] {
    let mut bins: [Vec<(f32, f32, f32)>; 8] = Default::default();
    let stride = (img.len() / 60_000).max(1);
    for i in (0..img.len()).step_by(stride) {
        let lab = linear_to_lab(img.pixel(i));
        let bin = ((lab[0] / 100.0 * 8.0) as usize).min(7);
        let chroma = (lab[1] * lab[1] + lab[2] * lab[2]).sqrt();
        bins[bin].push((chroma, lab[1], lab[2]));
    }
    let mut out = [0.0f32; 8];
    for (i, b) in bins.iter_mut().enumerate() {
        if b.len() < 10 {
            continue;
        }
        b.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(std::cmp::Ordering::Equal));
        let take = (b.len() / 10).max(3);
        let (mut sa, mut sb) = (0.0f64, 0.0f64);
        for (_, a, bb) in b.iter().take(take) {
            sa += *a as f64;
            sb += *bb as f64;
        }
        let ca = (sa / take as f64) as f32;
        let cb = (sb / take as f64) as f32;
        out[i] = (ca * ca + cb * cb).sqrt();
    }
    out
}

/// 中性轴**相对参考**的漂移：每段只统计参考图里彩度最低 10% 的像素，
/// 返回输出与参考在该像素集上的 (a,b) 均值差模长。这才是"色偏"的可比度量。
pub fn neutral_axis_shift(actual: &FloatImage, reference: &FloatImage) -> [f32; 8] {
    let mut bins: [Vec<(f32, f32, f32, f32, f32)>; 8] = Default::default();
    let stride = (actual.len() / 60_000).max(1);
    for i in (0..actual.len()).step_by(stride) {
        let lr = linear_to_lab(reference.pixel(i));
        let la = linear_to_lab(actual.pixel(i));
        let bin = ((lr[0] / 100.0 * 8.0) as usize).min(7);
        let c = (lr[1] * lr[1] + lr[2] * lr[2]).sqrt();
        bins[bin].push((c, lr[1], lr[2], la[1], la[2]));
    }
    let mut out = [0.0f32; 8];
    for (i, b) in bins.iter_mut().enumerate() {
        if b.len() < 10 {
            continue;
        }
        b.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(std::cmp::Ordering::Equal));
        let take = (b.len() / 10).max(3);
        let (mut ra, mut rb, mut aa, mut ab) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for (_, x1, x2, y1, y2) in b.iter().take(take) {
            ra += *x1 as f64;
            rb += *x2 as f64;
            aa += *y1 as f64;
            ab += *y2 as f64;
        }
        let n = take as f64;
        let da = (aa / n - ra / n) as f32;
        let db = (ab / n - rb / n) as f32;
        out[i] = (da * da + db * db).sqrt();
    }
    out
}

/// 只在"参考图本身是中性"的像素上算 ΔE00 —— 验收色偏最直接的指标。
pub fn mean_delta_e00_on_reference_neutrals(
    actual: &FloatImage,
    reference: &FloatImage,
    chroma_threshold: f32,
) -> (f32, usize) {
    let stride = (actual.len() / 50_000).max(1);
    let (mut sum, mut cnt) = (0.0f64, 0usize);
    for i in (0..actual.len()).step_by(stride) {
        let lr = linear_to_lab(reference.pixel(i));
        let c = (lr[1] * lr[1] + lr[2] * lr[2]).sqrt();
        if c <= chroma_threshold {
            let la = linear_to_lab(actual.pixel(i));
            sum += delta_e00(la, lr) as f64;
            cnt += 1;
        }
    }
    ((sum / cnt.max(1) as f64) as f32, cnt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_images_have_high_psnr() {
        let mut a = FloatImage::new(8, 8);
        for i in 0..a.len() {
            a.set_pixel(i, [0.5, 0.4, 0.3]);
        }
        let b = a.clone();
        assert!(psnr(&a, &b) > 90.0);
    }

    #[test]
    fn delta_e_of_identical_lab_is_zero() {
        assert!(delta_e00([50.0, 10.0, -5.0], [50.0, 10.0, -5.0]) < 1e-3);
    }

    #[test]
    fn white_is_neutral_in_lab() {
        let lab = linear_to_lab([1.0, 1.0, 1.0]);
        assert!((lab[0] - 100.0).abs() < 1.0, "{lab:?}");
        assert!(lab[1].abs() < 1.0 && lab[2].abs() < 1.0, "{lab:?}");
    }
}
