//! 伪 signature 生成与 message ID（Protobuf/varint 编码）

use base64::Engine;

/// 生成符合 Anthropic 官方格式的 message ID
///
/// 格式: `msg_01` + 22 位 base62 字符（大小写字母 + 数字）
pub(crate) fn generate_message_id() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let suffix: String = (0..22)
        .map(|_| CHARSET[fastrand::usize(..CHARSET.len())] as char)
        .collect();
    format!("msg_01{}", suffix)
}

/// 生成格式正确的伪 signature（base64 编码的 Protobuf 结构）
///
/// 基于逆向官方 API 响应得到的精确 Protobuf 结构：
/// ```text
/// Outer: field 2 (LEN, inner_payload) + field 3 (varint, 1)
/// Inner:
///   field 1 (LEN, metadata):
///     field 1 (varint): 14
///     field 3 (varint): 2
///     field 5 (bytes, 64): random nonce
///     field 6 (string): model name
///     field 7 (varint): 0
///     field 8 (string): "thinking"
///   field 2 (bytes, 12): random
///   field 3 (bytes, 12): random
///   field 4 (bytes, 48): random (HMAC?)
///   field 5 (bytes, 130-210): random (crypto signature)
/// ```
pub(crate) fn generate_fake_signature_for_model(model: &str) -> String {
    // --- 构建 metadata (inner field 1) ---
    let model_bytes = model.as_bytes();
    let block_type = b"thinking";

    let mut metadata: Vec<u8> = Vec::with_capacity(128);
    // field 1 (varint): 14
    metadata.extend_from_slice(&[0x08, 0x0E]);
    // field 3 (varint): 2
    metadata.extend_from_slice(&[0x18, 0x02]);
    // field 5 (bytes, 64): random nonce
    metadata.push(0x2A); // tag: field 5, wire type 2
    metadata.push(0x40); // length: 64
    for _ in 0..64 {
        metadata.push(fastrand::u8(..));
    }
    // field 6 (string): model name
    metadata.push(0x32); // tag: field 6, wire type 2
    metadata.push(model_bytes.len() as u8);
    metadata.extend_from_slice(model_bytes);
    // field 7 (varint): 0
    metadata.extend_from_slice(&[0x38, 0x00]);
    // field 8 (string): "thinking"
    metadata.push(0x42); // tag: field 8, wire type 2
    metadata.push(block_type.len() as u8);
    metadata.extend_from_slice(block_type);

    // --- 构建 inner payload ---
    let sig_len = fastrand::usize(130..=200); // 官方范围 132-208
    let mut inner: Vec<u8> = Vec::with_capacity(metadata.len() + 12 + 12 + 48 + sig_len + 10);

    // field 1 (LEN): metadata
    inner.push(0x0A); // tag: field 1, wire type 2
    encode_varint(&mut inner, metadata.len() as u64);
    inner.extend_from_slice(&metadata);

    // field 2 (bytes, 12): random
    inner.push(0x12); // tag: field 2, wire type 2
    inner.push(0x0C); // length: 12
    for _ in 0..12 {
        inner.push(fastrand::u8(..));
    }

    // field 3 (bytes, 12): random
    inner.push(0x1A); // tag: field 3, wire type 2
    inner.push(0x0C); // length: 12
    for _ in 0..12 {
        inner.push(fastrand::u8(..));
    }

    // field 4 (bytes, 48): random
    inner.push(0x22); // tag: field 4, wire type 2
    inner.push(0x30); // length: 48
    for _ in 0..48 {
        inner.push(fastrand::u8(..));
    }

    // field 5 (bytes, variable): crypto signature
    inner.push(0x2A); // tag: field 5, wire type 2
    encode_varint(&mut inner, sig_len as u64);
    for _ in 0..sig_len {
        inner.push(fastrand::u8(..));
    }

    // --- 构建 outer ---
    let mut buf: Vec<u8> = Vec::with_capacity(inner.len() + 6);
    // outer field 2 (LEN): inner payload
    buf.push(0x12); // tag: field 2, wire type 2
    encode_varint(&mut buf, inner.len() as u64);
    buf.extend_from_slice(&inner);
    // outer field 3 (varint): 1
    buf.extend_from_slice(&[0x18, 0x01]);

    base64::engine::general_purpose::STANDARD.encode(&buf)
}

/// 向 buf 追加 varint 编码
fn encode_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            break;
        } else {
            buf.push(byte | 0x80);
        }
    }
}
