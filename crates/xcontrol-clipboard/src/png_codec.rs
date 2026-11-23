//! PNG 编解码（FR-4.2 图片线格式，docs/04 §2）：平台 RGBA 内存形态 ↔ wire
//! `image/png` 字节流的纯逻辑转换层（跨平台编译、单测在所有开发机/CI 可跑——
//! newline.rs 先例：平台消费的数据变换放顶层模块）。
//!
//! # 为什么是本模块（而非各平台内嵌 / image crate / 手写）
//!
//! - **统一 wire 边界**：docs/04 §2 把 `image/png` 钉为图片交换唯一必选线格式；
//!   各平台本地格式（mac TIFF/NSImage、win CF_DIB、x11 image/png target）与
//!   wire 的转换都收敛到本模块，平台层只做「decode → RGBA → arboard」与
//!   「arboard RGBA → encode」两步。
//! - **png crate 而非 image crate**：`image` 0.25 编解码面（jpeg/gif/webp…）
//!   远大于需求；`png` 0.18 是纯 Rust 编解码器，且已在 gpui→image 传递树
//!   （0.18.1 同版，零新增包）——裁定记录见 workspace Cargo.toml png 条目。
//! - **非手写**：PNG 的 zlib/fdeflate 压缩管线不可手写维护。
//!
//! # RGBA 规范形与回声指纹（docs/04 §5 防线 1 的图片扩展）
//!
//! PNG 是**可重编码**容器：同一像素经 mac NSImage/TIFF 或 win CF_DIB 转换后
//! 重新编码，PNG 字节必然不同（压缩器参数非规范）。因此图片内容指纹取
//! **解码后的 RGBA 像素规范形**（[`rgba_canonical_fingerprint_input`]：宽高 +
//! 像素），本机写入与读回的像素 round-trip 无损（8bit RGBA 逐像素保持），
//! 指纹一致——与「Windows 指纹字节教训」同型：指纹必须取**读回形态的
//! round-trip 不动点**，而非落板字节。透明度例外见模块尾注。
//!
//! # 解码规范化
//!
//! [`decode_png_to_rgba`] 以 `Transformations::normalize_to_color8()` 把任意
//! PNG 色型（RGB/灰度/调色板/16bit）归一化到 8bit，再统一转 RGBA8（缺 alpha
//! 补 255）。编码端固定 RGBA8 输入。

use crate::error::ClipboardError;

/// PNG 解码规范化变换：EXPAND（调色板/灰度→RGB(A)/灰度）+ STRIP_16（16bit→8bit）
/// 的组合（png 0.18 语义：normalize_to_color8 的等价常量组合——该方法非 const，
/// bitflags 组合可在 const 上下文构造）。
const NORMALIZE_TO_8BIT: png::Transformations =
    png::Transformations::EXPAND.union(png::Transformations::STRIP_16);

/// RGBA8 像素编码为 PNG 字节（wire 格式，docs/04 §2）。
///
/// `rgba.len()` 必须等于 `width * height * 4`（不匹配返回
/// [`ClipboardError::ImageFormat`]，不做截断/补齐——同步管线不修图）。
///
/// # Errors
/// [`ClipboardError::ImageFormat`]：像素长度与宽高不符 / png 编码失败。
pub fn encode_rgba_to_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ClipboardError> {
    let expected = usize::try_from(u64::from(width) * u64::from(height) * 4).map_err(|_| {
        ClipboardError::ImageFormat {
            reason: format!("宽高乘积溢出（{width}x{height}）"),
        }
    })?;
    if rgba.len() != expected {
        return Err(ClipboardError::ImageFormat {
            reason: format!(
                "RGBA 像素长度 {} 与宽高 {width}x{height} 不符（期望 {expected}）",
                rgba.len()
            ),
        });
    }
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| ClipboardError::ImageFormat {
                reason: format!("PNG 头写入失败：{error}"),
            })?;
        writer
            .write_image_data(rgba)
            .map_err(|error| ClipboardError::ImageFormat {
                reason: format!("PNG 像素写入失败：{error}"),
            })?;
        // writer 落地作用域尾部 flush（finish 由 Drop 收敛到 &mut encoded）
    }
    Ok(encoded)
}

/// PNG 字节解码为 RGBA8 像素（返回 `(width, height, rgba)`）。
///
/// 输入接受任意色型/位深的 PNG（normalize_to_color8 归一化后统一转 RGBA8）；
/// 超大尺寸防线上限 [`MAX_DECODE_DIMENSION`]（协议 16MiB 上限下不可能是真实
/// 图片的合理边长，防御畸形 PNG 的内存放大——解析头部即拒）。
///
/// # Errors
/// [`ClipboardError::ImageFormat`]：PNG 解析失败 / 边长超防线上限 / 色型不支持。
pub fn decode_png_to_rgba(png_bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), ClipboardError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    // 变换须在 Decoder 上设置（png 0.18：Reader 无 set_transformations——
    // 归一化是解码前置参数，read_info 产出 Reader 时已固化）。
    decoder.set_transformations(NORMALIZE_TO_8BIT);
    // read_info 读头 + 像素流元数据并产出 Reader（png 0.18：read_header_info 是
    // 中间态、读像素必须 read_info）。
    let mut reader = decoder
        .read_info()
        .map_err(|error| ClipboardError::ImageFormat {
            reason: format!("PNG 头解析失败：{error}"),
        })?;
    let (width, height) = reader.info().size();
    if width > MAX_DECODE_DIMENSION || height > MAX_DECODE_DIMENSION {
        return Err(ClipboardError::ImageFormat {
            reason: format!(
                "PNG 边长 {width}x{height} 超防线 {MAX_DECODE_DIMENSION}（畸形输入防御）"
            ),
        });
    }
    let mut buffer = Vec::new();
    // png 0.18 的 next_frame 要求调用方**预分配足额缓冲**（0.17 起的 API 变化：
    // buf.len() < output_buffer_size 直接报 ImageBufferSize 参数错）——按变换后
    // 的输出尺寸精确分配（output_buffer_size 已计入色型变换）。
    let output_len = reader
        .output_buffer_size()
        .ok_or(ClipboardError::ImageFormat {
            reason: "PNG 输出缓冲尺寸计算溢出（尺寸异常）".into(),
        })?;
    buffer.resize(output_len, 0);
    match reader.next_frame(&mut buffer) {
        Ok(info) => {
            let _ = info;
        }
        Err(error) => {
            return Err(ClipboardError::ImageFormat {
                reason: format!("PNG 像素解码失败：{error}"),
            });
        }
    }
    let rgba = match reader.output_color_type() {
        (png::ColorType::Rgba, png::BitDepth::Eight) => buffer,
        (png::ColorType::Rgb, png::BitDepth::Eight) => rgb8_to_rgba8(&buffer),
        (png::ColorType::Grayscale, png::BitDepth::Eight) => gray8_to_rgba8(&buffer),
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => gray_alpha8_to_rgba8(&buffer),
        (color, depth) => {
            return Err(ClipboardError::ImageFormat {
                reason: format!(
                    "不支持的 PNG 输出色型 {color:?}/{depth:?}（normalize 后仍非 8bit）"
                ),
            });
        }
    };
    Ok((width, height, rgba))
}

/// 解码边长防线上限：65535（u16 满值；真实截图远小于此，畸形 PNG 在分配
/// 像素缓冲前即拒——防止头部合法、像素流放大的内存 DoS，docs/04 §6 资源纪律）。
pub const MAX_DECODE_DIMENSION: u32 = 65_535;

/// 图片指纹规范形输入（docs/04 §5 防线 1 图片扩展的判据字节）。
///
/// 布局：`b"XCLIP-IMG" + width(u32 BE) + height(u32 BE) + rgba`——魔数隔离
/// 其他格式指纹输入、宽高绑进哈希（同像素不同尺寸不碰撞）。
/// [`crate::fingerprint_bytes`] 对本输出取 blake3 短哈希即图片内容指纹。
#[must_use]
pub fn rgba_canonical_fingerprint_input(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 8 + rgba.len());
    out.extend_from_slice(b"XCLIP-IMG");
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(rgba);
    out
}

/// PNG 字节的图片指纹规范形输入（decode 后取 [`rgba_canonical_fingerprint_input`]）。
///
/// # Errors
/// [`ClipboardError::ImageFormat`]：PNG 解析失败（同 [`decode_png_to_rgba`]）。
pub fn png_canonical_fingerprint_input(png_bytes: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    let (width, height, rgba) = decode_png_to_rgba(png_bytes)?;
    Ok(rgba_canonical_fingerprint_input(width, height, &rgba))
}

/// RGB8 → RGBA8（补不透明 alpha）。
fn rgb8_to_rgba8(rgb: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((rgb.len() / 3) * 4);
    for [r, g, b] in rgb.as_chunks::<3>().0 {
        rgba.extend_from_slice(&[*r, *g, *b, 255]);
    }
    rgba
}

/// 灰度 8bit → RGBA8（灰值复制三通道 + 不透明 alpha）。
fn gray8_to_rgba8(gray: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(gray.len() * 4);
    for &value in gray {
        rgba.extend_from_slice(&[value, value, value, 255]);
    }
    rgba
}

/// 灰度+alpha 8bit → RGBA8。
fn gray_alpha8_to_rgba8(gray_alpha: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(gray_alpha.len() * 2);
    for &[value, alpha] in gray_alpha.as_chunks::<2>().0 {
        rgba.extend_from_slice(&[value, value, value, alpha]);
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint_bytes;

    /// 4×2 确定性测试像素（含渐变与两种 alpha）。
    fn sample_rgba() -> (u32, u32, Vec<u8>) {
        let mut rgba = Vec::new();
        for y in 0u8..2 {
            for x in 0u8..4 {
                rgba.extend_from_slice(&[x * 60, y * 100, 128, if x == 3 { 0 } else { 255 }]);
            }
        }
        (4, 2, rgba)
    }

    /// encode → decode round-trip：宽高与像素逐字节一致（8bit RGBA 无损）。
    #[test]
    fn encode_decode_round_trip_is_pixel_exact() {
        let (width, height, rgba) = sample_rgba();
        let png = encode_rgba_to_png(&rgba, width, height).expect("编码");
        let (w2, h2, rgba2) = decode_png_to_rgba(&png).expect("解码");
        assert_eq!((w2, h2), (width, height));
        assert_eq!(rgba2, rgba, "8bit RGBA round-trip 逐像素无损");
    }

    /// 编码确定性：同像素两次编码字节一致（PNG 输出确定，png 0.18 无随机参数）
    /// ——指纹"写侧两次编码同指纹"的依据。
    #[test]
    fn encoding_is_deterministic() {
        let (width, height, rgba) = sample_rgba();
        let a = encode_rgba_to_png(&rgba, width, height).expect("编码 a");
        let b = encode_rgba_to_png(&rgba, width, height).expect("编码 b");
        assert_eq!(a, b);
    }

    /// PNG 字节级差异不改变指纹规范形（重编码场景——mac NSImage/TIFF 读回
    /// 重编码的等价性依据）。
    #[test]
    fn reencoded_png_yields_same_fingerprint_input() {
        let (width, height, rgba) = sample_rgba();
        let png_a = encode_rgba_to_png(&rgba, width, height).expect("编码 a");
        // 用不同压缩档位重编码（模拟对端/平台重编码）：png 0.18 Compression
        // 默认 vs Fast——字节不同、像素相同。
        let mut png_b = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_b, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Fast);
            let mut writer = encoder.write_header().expect("头");
            writer.write_image_data(&rgba).expect("像素");
        }
        let input_a = png_canonical_fingerprint_input(&png_a).expect("指纹 a");
        let input_b = png_canonical_fingerprint_input(&png_b).expect("指纹 b");
        assert_ne!(png_a, png_b, "两档压缩应产生不同 PNG 字节");
        assert_eq!(
            fingerprint_bytes(&input_a),
            fingerprint_bytes(&input_b),
            "重编码后指纹规范形一致（像素 round-trip 不动点）"
        );
    }

    /// 非 RGBA 色型解码归一化：RGB 补 alpha、灰度复制、灰度+alpha 扩展。
    #[test]
    fn decode_normalizes_other_color_types() {
        let (width, height, rgba) = sample_rgba();

        // RGB（无 alpha）
        let mut rgb = Vec::new();
        for &[r, g, b, _] in rgba.as_chunks::<4>().0 {
            rgb.extend_from_slice(&[r, g, b]);
        }
        let png_rgb = {
            let mut out = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut out, width, height);
                encoder.set_color(png::ColorType::Rgb);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().expect("头");
                writer.write_image_data(&rgb).expect("像素");
                // writer Drop 落 IEND；作用域先收束再取 out（借用检查）
            }
            out
        };
        let (_, _, rgba_from_rgb) = decode_png_to_rgba(&png_rgb).expect("解码 RGB");
        // RGB 载荷不含 alpha：解码补 255 后与"原像素强制不透明"一致
        // （alpha 通道在 RGB 化时丢弃属预期，等价判据是 RGB 三通道）。
        let expected_opaque: Vec<u8> = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|[r, g, b, _]| [*r, *g, *b, 255u8])
            .collect();
        assert_eq!(rgba_from_rgb, expected_opaque, "RGB 解码补 255 alpha");

        // 灰度 8bit
        let png_gray = {
            let mut out = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut out, width, height);
                encoder.set_color(png::ColorType::Grayscale);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().expect("头");
                let pixel_count = (u64::from(width) * u64::from(height)) as usize;
                let gray: Vec<u8> = (0..pixel_count).map(|i| (i % 251) as u8).collect();
                writer.write_image_data(&gray).expect("像素");
            }
            out
        };
        let (w, h, rgba_from_gray) = decode_png_to_rgba(&png_gray).expect("解码灰度");
        assert_eq!((w, h), (width, height));
        assert_eq!(rgba_from_gray.len(), rgba.len());
        let pixel_count = (u64::from(width) * u64::from(height)) as usize;
        for (pixel, value) in rgba_from_gray
            .as_chunks::<4>()
            .0
            .iter()
            .zip((0..pixel_count).map(|i| (i % 251) as u8))
        {
            assert_eq!(*pixel, [value, value, value, 255]);
        }
    }

    /// 像素长度不符 / 畸形 PNG / 超边长防御：干净报错不 panic。
    #[test]
    fn invalid_inputs_error_cleanly() {
        let err = encode_rgba_to_png(&[0u8; 3], 1, 1).unwrap_err();
        assert!(
            matches!(err, ClipboardError::ImageFormat { .. }),
            "长度不符"
        );

        let err = decode_png_to_rgba(b"not a png").unwrap_err();
        assert!(
            matches!(err, ClipboardError::ImageFormat { .. }),
            "畸形 PNG"
        );

        // 截断的合法头：像素流损坏
        let (width, height, rgba) = sample_rgba();
        let mut png = encode_rgba_to_png(&rgba, width, height).expect("编码");
        png.truncate(png.len() / 2);
        let err = decode_png_to_rgba(&png).unwrap_err();
        assert!(
            matches!(err, ClipboardError::ImageFormat { .. }),
            "截断 PNG"
        );
    }

    /// 指纹规范形：同像素同指纹、差一像素差指纹、宽高参与哈希。
    #[test]
    fn fingerprint_input_discriminates() {
        let (width, height, rgba) = sample_rgba();
        let input = rgba_canonical_fingerprint_input(width, height, &rgba);
        assert_eq!(
            fingerprint_bytes(&input),
            fingerprint_bytes(&rgba_canonical_fingerprint_input(width, height, &rgba)),
            "同像素同指纹"
        );
        let mut altered = rgba.clone();
        altered[0] ^= 1;
        assert_ne!(
            fingerprint_bytes(&input),
            fingerprint_bytes(&rgba_canonical_fingerprint_input(width, height, &altered)),
            "差一像素差指纹"
        );
        assert_ne!(
            fingerprint_bytes(&input),
            fingerprint_bytes(&rgba_canonical_fingerprint_input(width, height + 1, &rgba)),
            "宽高参与哈希（同像素不同尺寸不碰撞）"
        );
    }
}
