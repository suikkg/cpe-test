//! 极简 HTTP/1.1 客户端（零第三方依赖）。
//! 对端是我们自己的 tiny_http agent。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// POST JSON，返回 (状态码, 响应体)
pub fn post_json(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
    timeout: Duration,
) -> Result<(u16, String), String> {
    request("POST", host, port, path, Some(body), timeout)
}

pub fn get(host: &str, port: u16, path: &str, timeout: Duration) -> Result<(u16, String), String> {
    request("GET", host, port, path, None, timeout)
}

fn request(
    method: &str,
    host: &str,
    port: u16,
    path: &str,
    body: Option<&str>,
    timeout: Duration,
) -> Result<(u16, String), String> {
    let addr_str = format!("{host}:{port}");
    let addrs: Vec<_> = addr_str
        .to_socket_addrs()
        .map_err(|e| format!("解析地址 {addr_str} 失败: {e}"))?
        .collect();
    let addr = addrs
        .first()
        .ok_or_else(|| format!("地址 {addr_str} 无法解析"))?;

    let mut stream = TcpStream::connect_timeout(addr, CONNECT_TIMEOUT)
        .map_err(|e| format!("连接 {addr_str} 失败: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;

    let b = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
        b.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("发送请求失败: {e}"))?;

    // 读响应头
    let mut reader = BufReader::new(&mut stream);
    let mut head_lines: Vec<String> = Vec::new();
    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        let n = reader
            .read_line(&mut line_buf)
            .map_err(|e| format!("读头失败: {e}"))?;
        if n == 0 {
            return Err("连接意外关闭".into());
        }
        let l = line_buf.trim_end().to_string();
        if l.is_empty() {
            break; // 空行 = 头结束
        }
        head_lines.push(l);
    }

    let head_text = head_lines.join("\r\n");
    let status = parse_status(head_lines.first().map(String::as_str).unwrap_or(""))?;
    let is_chunked = head_text
        .to_lowercase()
        .contains("transfer-encoding: chunked");

    // 读响应体
    let body_str = if is_chunked {
        read_chunked_body(&mut reader)?
    } else {
        let cl = parse_content_length(&head_text);
        let mut buf = Vec::new();
        if let Some(len) = cl {
            buf.resize(len, 0);
            reader
                .read_exact(&mut buf)
                .map_err(|e| format!("读响应体失败: {e}"))?;
        } else {
            reader
                .read_to_end(&mut buf)
                .map_err(|e| format!("读响应体失败: {e}"))?;
        }
        String::from_utf8_lossy(&buf).into_owned()
    };

    Ok((status, body_str))
}

/// 解码 chunked transfer encoding
fn read_chunked_body<R: BufRead>(reader: &mut R) -> Result<String, String> {
    let mut out: Vec<u8> = Vec::new();
    let mut size_buf = String::new();
    loop {
        size_buf.clear();
        reader
            .read_line(&mut size_buf)
            .map_err(|e| format!("读 chunk 大小失败: {e}"))?;
        let size_str = size_buf.trim();
        // 可能有 chunk extension (;...)
        let size_str = size_str.split(';').next().unwrap_or(size_str);
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|e| format!("chunk 大小解析失败 [{size_str}]: {e}"))?;
        if size == 0 {
            // 最后的空 chunk，读掉尾部 \r\n
            let _ = reader.read_line(&mut String::new());
            break;
        }
        let mut chunk_data = vec![0u8; size];
        reader
            .read_exact(&mut chunk_data)
            .map_err(|e| format!("读 chunk 数据失败: {e}"))?;
        out.extend_from_slice(&chunk_data);
        // 读掉 chunk 后的 \r\n
        let _ = reader.read_line(&mut String::new());
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn parse_content_length(head: &str) -> Option<usize> {
    for line in head.lines() {
        let l = line.to_lowercase();
        if let Some(v) = l.strip_prefix("content-length:") {
            return v.trim().parse().ok();
        }
    }
    None
}

fn parse_status(line: &str) -> Result<u16, String> {
    let mut parts = line.split_whitespace();
    let _ver = parts.next();
    parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("无法解析状态行: {line}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_helpers() {
        assert_eq!(
            parse_content_length("HTTP/1.1 200 OK\r\ncontent-length: 12\r\n"),
            Some(12)
        );
        assert_eq!(parse_status("HTTP/1.1 200 OK").unwrap(), 200);
    }

    #[test]
    fn test_roundtrip_with_tiny_http() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for mut rq in server.incoming_requests() {
                let mut body = String::new();
                let _ = rq.as_reader().read_to_string(&mut body);
                let resp = tiny_http::Response::from_string(format!("echo:{body}"));
                let _ = rq.respond(resp);
            }
        });
        let (st, body) = post_json(
            "127.0.0.1",
            port,
            "/test",
            "{\"a\":1}",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(st, 200);
        assert_eq!(body, "echo:{\"a\":1}");
    }
}
