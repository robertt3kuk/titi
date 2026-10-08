use std::io::{self, BufRead, Write};
use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

use crate::{Genome, Severity};

const MAX_BODY: usize = 8 * 1024 * 1024;

/// Serves document symbols, definition, references, and diagnostics over LSP
/// stdio framing. One index at start. Does not spawn a process or bind a socket.
pub fn serve_lsp(root: &Path, reader: impl BufRead, writer: impl Write) -> io::Result<()> {
    let genome = Genome::index(root)?;
    let mut reader = reader;
    let mut writer = writer;
    loop {
        let Some(body) = read_frame(&mut reader)? else {
            return Ok(());
        };
        let message: Value = serde_json::from_slice(&body).map_err(invalid)?;
        if !message.is_object() {
            return Err(invalid("lsp body is not an object"));
        }
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("lsp message has no method"))?;
        if method == "exit" {
            return Ok(());
        }
        let id = message.get("id").filter(|id| !id.is_null());
        if method == "initialized" || id.is_none() {
            continue;
        }
        let id = id.ok_or_else(|| invalid("missing request id"))?;
        let result = match method {
            "initialize" => initialize_result(),
            "shutdown" => Value::Null,
            "textDocument/documentSymbol" => document_symbols(&genome, root, &message),
            "textDocument/definition" => definition(&genome, root, &message),
            "textDocument/references" => references(&genome, root, &message),
            "textDocument/diagnostic" => diagnostic(&genome, root, &message),
            _ => {
                write_frame(
                    &mut writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": "method not found" }
                    }),
                )?;
                continue;
            }
        };
        write_frame(
            &mut writer,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            }),
        )?;
    }
}

/// The handshake, including what this index can actually do per language.
///
/// The level roster rides in `experimental`: a client that draws it can tell
/// its user that a Java symbol is a guess before they trust it, and a client
/// that ignores it loses nothing. The per-file `diagnostic` reply does not
/// repeat it — that channel answers "what is wrong with this file", and a
/// constant hint in every file is noise.
fn initialize_result() -> Value {
    json!({
        "capabilities": {
            "documentSymbolProvider": true,
            "definitionProvider": true,
            "referencesProvider": true,
            "diagnosticProvider": true
        },
        "serverInfo": { "name": "titi-genome" },
        "experimental": {
            "titiGenome": {
                "languages": Genome::capabilities()
                    .iter()
                    .map(|capability| {
                        json!({
                            "language": capability.language,
                            "level": capability.level.as_str()
                        })
                    })
                    .collect::<Vec<Value>>()
            }
        }
    })
}

fn document_symbols(genome: &Genome, root: &Path, message: &Value) -> Value {
    let Some(rel) = rel_of(root, message) else {
        return json!([]);
    };
    let symbols = genome.document_symbols(&rel);
    Value::Array(
        symbols
            .iter()
            .map(|site| {
                json!({
                    "name": site.name,
                    "kind": 12,
                    "location": lsp_location(root, &rel, site.line, site.character, site.name.len())
                })
            })
            .collect(),
    )
}

fn definition(genome: &Genome, root: &Path, message: &Value) -> Value {
    let Some((rel, line, character)) = position_of(root, message) else {
        return Value::Null;
    };
    genome
        .definition(&rel, line, character)
        .map(|loc| lsp_location(root, &loc.path, loc.line, loc.character, 0))
        .unwrap_or(Value::Null)
}

fn references(genome: &Genome, root: &Path, message: &Value) -> Value {
    let Some((rel, line, character)) = position_of(root, message) else {
        return json!([]);
    };
    Value::Array(
        genome
            .references(&rel, line, character)
            .into_iter()
            .map(|loc| lsp_location(root, &loc.path, loc.line, loc.character, 0))
            .collect(),
    )
}

fn diagnostic(genome: &Genome, root: &Path, message: &Value) -> Value {
    let Some(rel) = rel_of(root, message) else {
        return json!({ "kind": "full", "items": [] });
    };
    let items: Vec<Value> = genome
        .check()
        .into_iter()
        .filter(|item| item.path == rel)
        .map(|item| {
            json!({
                "range": lsp_range(item.line, item.character, 0),
                "severity": lsp_severity(item.severity),
                "code": item.code,
                "source": "titi-genome",
                "message": item.message
            })
        })
        .collect();
    json!({ "kind": "full", "items": items })
}

fn lsp_severity(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 1,
        Severity::Warning => 2,
        Severity::Info => 3,
    }
}

fn lsp_location(root: &Path, rel: &str, line: u32, character: u32, name_len: usize) -> Value {
    json!({
        "uri": file_uri(root, rel),
        "range": lsp_range(line, character, name_len)
    })
}

fn lsp_range(line: u32, character: u32, name_len: usize) -> Value {
    let lsp_line = line.saturating_sub(1);
    json!({
        "start": { "line": lsp_line, "character": character },
        "end": {
            "line": lsp_line,
            "character": character.saturating_add(u32::try_from(name_len).unwrap_or(0))
        }
    })
}

fn file_uri(root: &Path, rel: &str) -> String {
    format!("file://{}", root.join(rel).display())
}

fn position_of(root: &Path, message: &Value) -> Option<(String, u32, u32)> {
    let rel = rel_of(root, message)?;
    let position = message.pointer("/params/position")?;
    let line = position.get("line")?.as_u64()?;
    let character = position.get("character")?.as_u64()?;
    let line = u32::try_from(line).ok()?.saturating_add(1);
    let character = u32::try_from(character).ok()?;
    Some((rel, line, character))
}

fn rel_of(root: &Path, message: &Value) -> Option<String> {
    let uri = message.pointer("/params/textDocument/uri")?.as_str()?;
    uri_to_rel(root, uri)
}

fn uri_to_rel(root: &Path, uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    let abs = PathBuf::from(&decoded);
    let rel = abs.strip_prefix(root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    if rel
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return None;
    }
    let text = rel.to_str()?;
    Some(text.replace('\\', "/"))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[index + 1]), hex_val(bytes[index + 2])) {
                out.push((hi << 4) | lo);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut content_length = None;
    let mut saw_header = false;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            if !saw_header {
                return Ok(None);
            }
            return Err(invalid("truncated lsp header"));
        }
        saw_header = true;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(invalid("malformed lsp header"));
        };
        if name.eq_ignore_ascii_case("Content-Length") {
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|_| invalid("malformed Content-Length"))?;
            if parsed > MAX_BODY {
                return Err(invalid("lsp body too large"));
            }
            content_length = Some(parsed);
        }
    }
    let Some(len) = content_length else {
        return Err(invalid("missing Content-Length"));
    };
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_frame(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(invalid)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

fn invalid(error: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
