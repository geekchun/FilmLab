//! 平面式 f32 RGB 图像（通道优先布局，便于逐通道统计与 GPU 上传）。

use image::{DynamicImage, GenericImageView, ImageBuffer, Rgb, RgbImage, Rgb32FImage};

#[derive(Clone, Debug)]
pub struct FloatImage {
    pub width: usize,
    pub height: usize,
    /// 3 个平面：索引 `c * (w*h) + y*w + x`，线性光，通常 ∈ [0,1]
    pub data: Vec<f32>,
}

/// 输入解码时的伽马约定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Linearize {
    /// 数据已是线性（扫描仪 16-bit 线性 TIFF / 线性 DNG）
    None,
    /// 常规伽马编码，例如 2.2 / 1.8
    Gamma,
    /// sRGB 传输函数
    Srgb,
}

impl FloatImage {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            data: vec![0.0; width * height * 3],
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.width * self.height
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn plane(&self, c: usize) -> &[f32] {
        let n = self.len();
        &self.data[c * n..(c + 1) * n]
    }

    #[inline]
    pub fn plane_mut(&mut self, c: usize) -> &mut [f32] {
        let n = self.len();
        &mut self.data[c * n..(c + 1) * n]
    }

    #[inline]
    pub fn pixel(&self, i: usize) -> [f32; 3] {
        let n = self.len();
        [self.data[i], self.data[n + i], self.data[2 * n + i]]
    }

    #[inline]
    pub fn set_pixel(&mut self, i: usize, v: [f32; 3]) {
        let n = self.len();
        self.data[i] = v[0];
        self.data[n + i] = v[1];
        self.data[2 * n + i] = v[2];
    }

    /// 按整数因子降采样（box 均值），用于预览与统计。
    pub fn downscale(&self, factor: usize) -> FloatImage {
        if factor <= 1 || self.width < factor || self.height < factor {
            return self.clone();
        }
        let w = self.width / factor;
        let h = self.height / factor;
        let mut out = FloatImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0.0f32; 3];
                for dy in 0..factor {
                    for dx in 0..factor {
                        let p = self.pixel((y * factor + dy) * self.width + x * factor + dx);
                        acc[0] += p[0];
                        acc[1] += p[1];
                        acc[2] += p[2];
                    }
                }
                let inv = 1.0 / (factor * factor) as f32;
                out.set_pixel(y * w + x, [acc[0] * inv, acc[1] * inv, acc[2] * inv]);
            }
        }
        out
    }

    /// 从文件解码为线性 f32。
    pub fn load(path: &std::path::Path, lin: Linearize, gamma: f32) -> anyhow::Result<Self> {
        let img = image::open(path)?;
        Ok(Self::from_dynamic(&img, lin, gamma))
    }

    pub fn from_dynamic(img: &DynamicImage, lin: Linearize, gamma: f32) -> Self {
        let (w, h) = img.dimensions();
        let mut out = FloatImage::new(w as usize, h as usize);
        let n = out.len();
        let decode = |v: f32| -> f32 {
            match lin {
                Linearize::None => v,
                Linearize::Gamma => v.max(0.0).powf(gamma),
                Linearize::Srgb => srgb_to_linear(v),
            }
        };
        match img {
            DynamicImage::ImageRgb16(buf) => {
                for (i, px) in buf.pixels().enumerate() {
                    out.data[i] = decode(px[0] as f32 / 65535.0);
                    out.data[n + i] = decode(px[1] as f32 / 65535.0);
                    out.data[2 * n + i] = decode(px[2] as f32 / 65535.0);
                }
            }
            DynamicImage::ImageRgb8(buf) => {
                for (i, px) in buf.pixels().enumerate() {
                    out.data[i] = decode(px[0] as f32 / 255.0);
                    out.data[n + i] = decode(px[1] as f32 / 255.0);
                    out.data[2 * n + i] = decode(px[2] as f32 / 255.0);
                }
            }
            DynamicImage::ImageRgb32F(buf) => {
                for (i, px) in buf.pixels().enumerate() {
                    out.data[i] = decode(px[0]);
                    out.data[n + i] = decode(px[1]);
                    out.data[2 * n + i] = decode(px[2]);
                }
            }
            other => {
                let rgb = other.to_rgb16();
                for (i, px) in rgb.pixels().enumerate() {
                    out.data[i] = decode(px[0] as f32 / 65535.0);
                    out.data[n + i] = decode(px[1] as f32 / 65535.0);
                    out.data[2 * n + i] = decode(px[2] as f32 / 65535.0);
                }
            }
        }
        out
    }

    /// 编码为 16-bit TIFF（显示参考，直接量化到 0..65535）。
    pub fn save_tiff16(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let buf = self.to_rgb16();
        buf.save(path)?;
        Ok(())
    }

    /// 编码为 8-bit PNG/JPEG（按 sRGB 编码）。
    pub fn save_display8(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let mut buf: RgbImage = ImageBuffer::new(self.width as u32, self.height as u32);
        for (i, px) in buf.pixels_mut().enumerate() {
            let p = self.pixel(i);
            *px = Rgb([
                (clamp01(p[0]) * 255.0).round() as u8,
                (clamp01(p[1]) * 255.0).round() as u8,
                (clamp01(p[2]) * 255.0).round() as u8,
            ]);
        }
        buf.save(path)?;
        Ok(())
    }

    pub fn to_rgb16(&self) -> ImageBuffer<Rgb<u16>, Vec<u16>> {
        let mut buf: ImageBuffer<Rgb<u16>, Vec<u16>> =
            ImageBuffer::new(self.width as u32, self.height as u32);
        for (i, px) in buf.pixels_mut().enumerate() {
            let p = self.pixel(i);
            *px = Rgb([
                (clamp01(p[0]) * 65535.0).round() as u16,
                (clamp01(p[1]) * 65535.0).round() as u16,
                (clamp01(p[2]) * 65535.0).round() as u16,
            ]);
        }
        buf
    }

    pub fn to_rgb32f(&self) -> Rgb32FImage {
        let mut buf: Rgb32FImage = ImageBuffer::new(self.width as u32, self.height as u32);
        for (i, px) in buf.pixels_mut().enumerate() {
            let p = self.pixel(i);
            *px = Rgb(p);
        }
        buf
    }
}

#[inline]
pub fn clamp01(x: f32) -> f32 {
    if x < 0.0 {
        0.0
    } else if x > 1.0 {
        1.0
    } else {
        x
    }
}

#[inline]
pub fn srgb_to_linear(v: f32) -> f32 {
    let v = v.max(0.0);
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

#[inline]
pub fn linear_to_srgb(v: f32) -> f32 {
    let v = clamp01(v);
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_roundtrip() {
        for i in 0..=100 {
            let v = i as f32 / 100.0;
            let back = linear_to_srgb(srgb_to_linear(v));
            assert!((back - v).abs() < 2e-3, "{v} -> {back}");
        }
    }

    #[test]
    fn downscale_averages() {
        let mut img = FloatImage::new(4, 4);
        for i in 0..16 {
            img.set_pixel(i, [1.0, 0.5, 0.25]);
        }
        let d = img.downscale(2);
        assert_eq!((d.width, d.height), (2, 2));
        let p = d.pixel(0);
        assert!((p[0] - 1.0).abs() < 1e-6);
        assert!((p[1] - 0.5).abs() < 1e-6);
    }
}
