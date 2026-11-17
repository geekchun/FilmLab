//! film-core：C-41 彩色负片去色罩核心库。
//!
//! 管线（见 `pipeline`）：
//! 片基归一 → 相对密度 → 解耦矩阵 `A^-1` → 三层染料密度 → 虚拟印相 → 显示编码。
//!
//! 关键设计：色罩被当作**密度相关的量**（`b0` 只在片基处显现），
//! 而非常量橙色；串扰矩阵由整卷联合估计（`calib::fit_unmix`）。

pub mod calib;
pub mod image;
pub mod metrics;
pub mod model;
pub mod neg;
pub mod numeric;
pub mod pipeline;
pub mod print;
pub mod synth;

pub use calib::{BaseEstimate, BaseSource, GainFit};
pub use image::{FloatImage, Linearize};
pub use model::{NegativeModel, StockProfile};
pub use pipeline::{process, ProcessOutcome, ProcessParams};
pub use print::{auto_tone_params, AutoTone, Look, LookParams, PrintParams};
