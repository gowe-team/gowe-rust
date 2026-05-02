use ahash::HashMap;

use crate::{
    error::{RecurramError, Result},
    model::Value,
    wire::{Reader, encode_varuint},
};

const NULL_TAG: u8 = 0xC0;
const FALSE_TAG: u8 = 0xC1;
const TRUE_TAG: u8 = 0xC2;
const F64_TAG: u8 = 0xC3;
const U8_TAG: u8 = 0xC4;
const U16_TAG: u8 = 0xC5;
const U32_TAG: u8 = 0xC6;
const U64_TAG: u8 = 0xC7;
const I8_TAG: u8 = 0xC8;
const I16_TAG: u8 = 0xC9;
const I32_TAG: u8 = 0xCA;
const I64_TAG: u8 = 0xCB;
const BIN8_TAG: u8 = 0xCC;
const BIN16_TAG: u8 = 0xCD;
const BIN32_TAG: u8 = 0xCE;
const STR8_TAG: u8 = 0xCF;
const STR16_TAG: u8 = 0xD0;
const STR32_TAG: u8 = 0xD1;
const ARRAY16_TAG: u8 = 0xD2;
const ARRAY32_TAG: u8 = 0xD3;
const MAP16_TAG: u8 = 0xD4;
const MAP32_TAG: u8 = 0xD5;
const SHAPE_DEF_TAG: u8 = 0xD6;
const KEY_REF_TAG: u8 = 0xD8;
const STR_REF_TAG: u8 = 0xD9;

#[derive(Default)]
struct EncodeState {
    key_ids: HashMap<String, u64>,
    str_ids: HashMap<String, u64>,
    shape_ids: HashMap<Vec<String>, u64>,
    next_key_id: u64,
    next_str_id: u64,
    next_shape_id: u64,
}

#[derive(Default)]
struct DecodeState {
    keys: Vec<String>,
    strings: Vec<String>,
    shapes: Vec<Vec<String>>,
}

pub fn encode(value: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(256);
    let mut state = EncodeState::default();
    encode_value(value, &mut out, &mut state)?;
    Ok(out)
}

pub fn decode(bytes: &[u8]) -> Result<Value> {
    let mut reader = Reader::new(bytes);
    let mut state = DecodeState::default();
    let value = decode_value(&mut reader, &mut state)?;
    if !reader.is_eof() {
        return Err(RecurramError::InvalidData("trailing bytes in v2 decode"));
    }
    Ok(value)
}

fn encode_value(value: &Value, out: &mut Vec<u8>, state: &mut EncodeState) -> Result<()> {
    match value {
        Value::Null => out.push(NULL_TAG),
        Value::Bool(v) => out.push(if *v { TRUE_TAG } else { FALSE_TAG }),
        Value::I64(v) => encode_i64(*v, out),
        Value::U64(v) => encode_u64(*v, out),
        Value::F64(v) => {
            out.push(F64_TAG);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::String(s) => {
            if let Some(id) = state.str_ids.get(s).copied() {
                out.push(STR_REF_TAG);
                encode_varuint(id, out);
            } else {
                encode_string_literal(s, out);
                state.str_ids.insert(s.clone(), state.next_str_id);
                state.next_str_id += 1;
            }
        }
        Value::Binary(bytes) => encode_binary(bytes, out),
        Value::Array(values) => encode_array(values, out, state)?,
        Value::Map(entries) => encode_map(entries, out, state)?,
    }
    Ok(())
}

fn encode_array(values: &[Value], out: &mut Vec<u8>, state: &mut EncodeState) -> Result<()> {
    if let Some(shape_keys) = detect_shape_keys(values) {
        let shape_id = if let Some(id) = state.shape_ids.get(&shape_keys).copied() {
            id
        } else {
            let id = state.next_shape_id;
            state.next_shape_id += 1;
            state.shape_ids.insert(shape_keys.clone(), id);
            id
        };
        write_array_header(values.len(), out);
        out.push(SHAPE_DEF_TAG);
        encode_varuint(shape_id, out);
        encode_varuint(shape_keys.len() as u64, out);
        for key in &shape_keys {
            encode_key(key, out, state);
        }
        for value in values {
            if let Value::Map(entries) = value {
                // detect_shape_keys verified exact key order, so iterate directly
                for (_, field_value) in entries {
                    encode_value(field_value, out, state)?;
                }
            }
        }
        return Ok(());
    }

    write_array_header(values.len(), out);
    for value in values {
        encode_value(value, out, state)?;
    }
    Ok(())
}

fn encode_map(
    entries: &[(String, Value)],
    out: &mut Vec<u8>,
    state: &mut EncodeState,
) -> Result<()> {
    write_map_header(entries.len(), out);
    for (key, value) in entries {
        encode_key(key, out, state);
        encode_value(value, out, state)?;
    }
    Ok(())
}

fn encode_key(key: &str, out: &mut Vec<u8>, state: &mut EncodeState) {
    if let Some(id) = state.key_ids.get(key).copied() {
        out.push(KEY_REF_TAG);
        encode_varuint(id, out);
        return;
    }
    encode_string_literal(key, out);
    state.key_ids.insert(key.to_string(), state.next_key_id);
    state.next_key_id += 1;
}

fn encode_string_literal(value: &str, out: &mut Vec<u8>) {
    let bytes = value.as_bytes();
    if bytes.len() <= 31 {
        out.push(0x80 | (bytes.len() as u8));
    } else if u8::try_from(bytes.len()).is_ok() {
        out.push(STR8_TAG);
        out.push(bytes.len() as u8);
    } else if u16::try_from(bytes.len()).is_ok() {
        out.push(STR16_TAG);
        out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    } else {
        out.push(STR32_TAG);
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    }
    out.extend_from_slice(bytes);
}

fn encode_binary(value: &[u8], out: &mut Vec<u8>) {
    if u8::try_from(value.len()).is_ok() {
        out.push(BIN8_TAG);
        out.push(value.len() as u8);
    } else if u16::try_from(value.len()).is_ok() {
        out.push(BIN16_TAG);
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    } else {
        out.push(BIN32_TAG);
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    }
    out.extend_from_slice(value);
}

fn encode_u64(value: u64, out: &mut Vec<u8>) {
    if value <= 127 {
        out.push(value as u8);
    } else if u8::try_from(value).is_ok() {
        out.push(U8_TAG);
        out.push(value as u8);
    } else if u16::try_from(value).is_ok() {
        out.push(U16_TAG);
        out.extend_from_slice(&(value as u16).to_le_bytes());
    } else if u32::try_from(value).is_ok() {
        out.push(U32_TAG);
        out.extend_from_slice(&(value as u32).to_le_bytes());
    } else {
        out.push(U64_TAG);
        out.extend_from_slice(&value.to_le_bytes());
    }
}

fn encode_i64(value: i64, out: &mut Vec<u8>) {
    if (-32..=-1).contains(&value) {
        out.push((value as i8) as u8);
    } else if (0..=127).contains(&value) {
        out.push(value as u8);
    } else if i8::try_from(value).is_ok() {
        out.push(I8_TAG);
        out.push((value as i8) as u8);
    } else if i16::try_from(value).is_ok() {
        out.push(I16_TAG);
        out.extend_from_slice(&(value as i16).to_le_bytes());
    } else if i32::try_from(value).is_ok() {
        out.push(I32_TAG);
        out.extend_from_slice(&(value as i32).to_le_bytes());
    } else {
        out.push(I64_TAG);
        out.extend_from_slice(&value.to_le_bytes());
    }
}

fn write_array_header(len: usize, out: &mut Vec<u8>) {
    if len <= 15 {
        out.push(0xA0 | (len as u8));
    } else if u16::try_from(len).is_ok() {
        out.push(ARRAY16_TAG);
        out.extend_from_slice(&(len as u16).to_le_bytes());
    } else {
        out.push(ARRAY32_TAG);
        out.extend_from_slice(&(len as u32).to_le_bytes());
    }
}

fn write_map_header(len: usize, out: &mut Vec<u8>) {
    if len <= 15 {
        out.push(0xB0 | (len as u8));
    } else if u16::try_from(len).is_ok() {
        out.push(MAP16_TAG);
        out.extend_from_slice(&(len as u16).to_le_bytes());
    } else {
        out.push(MAP32_TAG);
        out.extend_from_slice(&(len as u32).to_le_bytes());
    }
}

fn detect_shape_keys(values: &[Value]) -> Option<Vec<String>> {
    if values.len() < 2 {
        return None;
    }
    let Value::Map(first) = values.first()? else {
        return None;
    };
    let keys: Vec<String> = first.iter().map(|(k, _)| k.clone()).collect();
    if keys.is_empty() {
        return None;
    }
    for value in values.iter().skip(1) {
        let Value::Map(entries) = value else {
            return None;
        };
        if entries.len() != keys.len() {
            return None;
        }
        for ((lhs, _), rhs) in entries.iter().zip(keys.iter()) {
            if lhs != rhs {
                return None;
            }
        }
    }
    Some(keys)
}

fn decode_value(reader: &mut Reader<'_>, state: &mut DecodeState) -> Result<Value> {
    let tag = reader.read_u8()?;
    decode_value_from_tag(reader, state, tag)
}

fn decode_value_from_tag(
    reader: &mut Reader<'_>,
    state: &mut DecodeState,
    tag: u8,
) -> Result<Value> {
    match tag {
        0x00..=0x7F => Ok(Value::U64(tag as u64)),
        0x80..=0x9F => {
            let len = (tag & 0x1F) as usize;
            let bytes = reader.read_exact(len)?;
            let s = std::str::from_utf8(bytes)
                .map_err(|_| RecurramError::Utf8Error)?
                .to_string();
            state.strings.push(s.clone());
            Ok(Value::String(s))
        }
        0xA0..=0xAF => {
            let len = (tag & 0x0F) as usize;
            decode_array_body(reader, state, len)
        }
        0xB0..=0xBF => {
            let len = (tag & 0x0F) as usize;
            decode_map_body(reader, state, len)
        }
        0xE0..=0xFF => Ok(Value::I64((tag as i8) as i64)),
        NULL_TAG => Ok(Value::Null),
        FALSE_TAG => Ok(Value::Bool(false)),
        TRUE_TAG => Ok(Value::Bool(true)),
        F64_TAG => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(reader.read_exact(8)?);
            Ok(Value::F64(f64::from_le_bytes(bytes)))
        }
        U8_TAG => Ok(Value::U64(reader.read_u8()? as u64)),
        U16_TAG => {
            let mut bytes = [0u8; 2];
            bytes.copy_from_slice(reader.read_exact(2)?);
            Ok(Value::U64(u16::from_le_bytes(bytes) as u64))
        }
        U32_TAG => {
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(reader.read_exact(4)?);
            Ok(Value::U64(u32::from_le_bytes(bytes) as u64))
        }
        U64_TAG => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(reader.read_exact(8)?);
            Ok(Value::U64(u64::from_le_bytes(bytes)))
        }
        I8_TAG => Ok(Value::I64((reader.read_u8()? as i8) as i64)),
        I16_TAG => {
            let mut bytes = [0u8; 2];
            bytes.copy_from_slice(reader.read_exact(2)?);
            Ok(Value::I64(i16::from_le_bytes(bytes) as i64))
        }
        I32_TAG => {
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(reader.read_exact(4)?);
            Ok(Value::I64(i32::from_le_bytes(bytes) as i64))
        }
        I64_TAG => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(reader.read_exact(8)?);
            Ok(Value::I64(i64::from_le_bytes(bytes)))
        }
        BIN8_TAG => {
            let len = reader.read_u8()? as usize;
            Ok(Value::Binary(reader.read_exact(len)?.to_vec()))
        }
        BIN16_TAG => {
            let mut len = [0u8; 2];
            len.copy_from_slice(reader.read_exact(2)?);
            Ok(Value::Binary(
                reader
                    .read_exact(u16::from_le_bytes(len) as usize)?
                    .to_vec(),
            ))
        }
        BIN32_TAG => {
            let mut len = [0u8; 4];
            len.copy_from_slice(reader.read_exact(4)?);
            Ok(Value::Binary(
                reader
                    .read_exact(u32::from_le_bytes(len) as usize)?
                    .to_vec(),
            ))
        }
        STR8_TAG | STR16_TAG | STR32_TAG => decode_string_tag(reader, state, tag),
        ARRAY16_TAG => {
            let mut len = [0u8; 2];
            len.copy_from_slice(reader.read_exact(2)?);
            decode_array_body(reader, state, u16::from_le_bytes(len) as usize)
        }
        ARRAY32_TAG => {
            let mut len = [0u8; 4];
            len.copy_from_slice(reader.read_exact(4)?);
            decode_array_body(reader, state, u32::from_le_bytes(len) as usize)
        }
        MAP16_TAG => {
            let mut len = [0u8; 2];
            len.copy_from_slice(reader.read_exact(2)?);
            decode_map_body(reader, state, u16::from_le_bytes(len) as usize)
        }
        MAP32_TAG => {
            let mut len = [0u8; 4];
            len.copy_from_slice(reader.read_exact(4)?);
            decode_map_body(reader, state, u32::from_le_bytes(len) as usize)
        }
        STR_REF_TAG => {
            let id = reader.read_varuint()? as usize;
            let Some(value) = state.strings.get(id).cloned() else {
                return Err(RecurramError::InvalidData("unknown str_ref id"));
            };
            Ok(Value::String(value))
        }
        // fixmap: 0xb0..=0xbf encodes a map with (tag & 0x0f) entries
        tag if (0xb0..=0xbf).contains(&tag) => {
            decode_map_body(reader, state, (tag & 0x0f) as usize)
        }
        _ => Err(RecurramError::InvalidTag(tag)),
    }
}

fn decode_string_tag(reader: &mut Reader<'_>, state: &mut DecodeState, tag: u8) -> Result<Value> {
    let len = match tag {
        STR8_TAG => reader.read_u8()? as usize,
        STR16_TAG => {
            let mut len = [0u8; 2];
            len.copy_from_slice(reader.read_exact(2)?);
            u16::from_le_bytes(len) as usize
        }
        STR32_TAG => {
            let mut len = [0u8; 4];
            len.copy_from_slice(reader.read_exact(4)?);
            u32::from_le_bytes(len) as usize
        }
        _ => unreachable!(),
    };
    let bytes = reader.read_exact(len)?;
    let s = std::str::from_utf8(bytes)
        .map_err(|_| RecurramError::Utf8Error)?
        .to_string();
    state.strings.push(s.clone());
    Ok(Value::String(s))
}

fn decode_array_body(
    reader: &mut Reader<'_>,
    state: &mut DecodeState,
    len: usize,
) -> Result<Value> {
    let mut values = Vec::with_capacity(len);
    if len == 0 {
        return Ok(Value::Array(values));
    }
    let first_tag = reader.read_u8()?;
    if first_tag == SHAPE_DEF_TAG {
        let shape_id = reader.read_varuint()? as usize;
        let key_count = reader.read_varuint()? as usize;
        let mut keys = Vec::with_capacity(key_count);
        for _ in 0..key_count {
            keys.push(decode_key(reader, state)?);
        }
        if shape_id >= state.shapes.len() {
            state.shapes.resize(shape_id + 1, Vec::new());
        }
        state.shapes[shape_id] = keys.clone();
        for _ in 0..len {
            let mut row = Vec::with_capacity(keys.len());
            for key in &keys {
                row.push((key.clone(), decode_value(reader, state)?));
            }
            values.push(Value::Map(row));
        }
        return Ok(Value::Array(values));
    }
    values.push(decode_value_from_tag(reader, state, first_tag)?);
    for _ in 1..len {
        values.push(decode_value(reader, state)?);
    }
    Ok(Value::Array(values))
}

fn decode_map_body(reader: &mut Reader<'_>, state: &mut DecodeState, len: usize) -> Result<Value> {
    let mut entries = Vec::with_capacity(len);
    for _ in 0..len {
        let key = decode_key(reader, state)?;
        let value = decode_value(reader, state)?;
        entries.push((key, value));
    }
    Ok(Value::Map(entries))
}

fn decode_key(reader: &mut Reader<'_>, state: &mut DecodeState) -> Result<String> {
    let tag = reader.read_u8()?;
    if tag == KEY_REF_TAG {
        let id = reader.read_varuint()? as usize;
        let Some(value) = state.keys.get(id).cloned() else {
            return Err(RecurramError::InvalidData("unknown key_ref id"));
        };
        return Ok(value);
    }
    if (0x80..=0x9F).contains(&tag) {
        let len = (tag & 0x1F) as usize;
        let bytes = reader.read_exact(len)?;
        let key = std::str::from_utf8(bytes)
            .map_err(|_| RecurramError::Utf8Error)?
            .to_string();
        state.keys.push(key.clone());
        return Ok(key);
    }
    if matches!(tag, STR8_TAG | STR16_TAG | STR32_TAG) {
        let Value::String(key) = decode_value_from_tag(reader, state, tag)? else {
            return Err(RecurramError::InvalidData("expected string key"));
        };
        state.keys.push(key.clone());
        return Ok(key);
    }
    Err(RecurramError::InvalidData(
        "map key must be key_ref or string",
    ))
}
