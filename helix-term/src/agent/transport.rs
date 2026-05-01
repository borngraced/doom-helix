use serde::Serialize;

pub fn encode_content_length_message<T: Serialize>(message: &T) -> anyhow::Result<Vec<u8>> {
    let body = serde_json::to_string(message)?;
    Ok(encode_json_content_length_message(&body))
}

pub fn encode_json_content_length_message(body: &str) -> Vec<u8> {
    let mut message = Vec::with_capacity(
        "Content-Length: \r\n\r\n".len() + body.len().to_string().len() + body.len(),
    );
    message.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    message.extend_from_slice(body.as_bytes());
    message
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn encodes_content_length_frame() {
        let message = encode_json_content_length_message(r#"{"jsonrpc":"2.0"}"#);
        assert_eq!(
            String::from_utf8(message).unwrap(),
            "Content-Length: 17\r\n\r\n{\"jsonrpc\":\"2.0\"}"
        );
    }

    #[test]
    fn encodes_serializable_message() {
        let message = encode_content_length_message(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize"
        }))
        .unwrap();

        let message = String::from_utf8(message).unwrap();
        assert!(message.starts_with("Content-Length: "));
        assert!(message.ends_with(r#""method":"initialize"}"#));
    }
}
