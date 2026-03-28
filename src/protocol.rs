use crate::{
    codec::{
        decode_f64_vector, decode_i64_vector, decode_u64_vector, encode_f64_vector,
        encode_i64_vector, encode_u64_vector,
    },
    error::{RecurramError, Result},
    model::{
        BaseRef, Column, ControlMessage, ControlOpcode, ControlStreamCodec, ElementType, KeyRef,
        MapEntry, Message, MessageKind, NullStrategy, PatchOpcode, PatchOperation, Schema,
        StringMode, TemplateDescriptor, TypedVector, TypedVectorData, Value, VectorCodec,
    },
    session::{
        DictionaryFallback, DictionaryProfile, SessionOptions, SessionState, UnknownReferencePolicy,
    },
    wire::{
        Reader, decode_zigzag, encode_bitmap, encode_bytes, encode_string, encode_varuint,
        encode_zigzag,
    },
};

const TAG_NULL: u8 = 0;
const TAG_BOOL_FALSE: u8 = 1;
const TAG_BOOL_TRUE: u8 = 2;
const TAG_I64: u8 = 3;
const TAG_U64: u8 = 4;
const TAG_F64: u8 = 5;
const TAG_STRING: u8 = 6;
const TAG_BINARY: u8 = 7;
const TAG_ARRAY: u8 = 8;
const TAG_MAP: u8 = 9;

#[derive(Debug, Clone, Default)]
pub struct RecurramCodec {
    pub state: SessionState,
}

impl RecurramCodec {
    pub fn with_options(options: SessionOptions) -> Self {
        Self {
            state: SessionState::with_options(options),
        }
    }

    pub fn encode_message(&mut self, message: &Message) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(256);
        self.write_message(message, &mut out)?;
        Ok(out)
    }

    pub fn decode_message(&mut self, bytes: &[u8]) -> Result<Message> {
        let mut reader = Reader::new(bytes);
        let msg = self.read_message(&mut reader)?;
        if !reader.is_eof() {
            return Err(RecurramError::InvalidData("trailing bytes in message"));
        }
        match &msg {
            Message::Control(_) => {}
            Message::StatePatch {
                base_ref,
                operations,
                literals,
            } => match self.apply_state_patch(base_ref, operations, literals) {
                Ok(reconstructed) => {
                    self.state.previous_message = Some(reconstructed);
                }
                Err(err @ RecurramError::UnknownReference(_, _))
                | Err(err @ RecurramError::StatelessRetryRequired(_, _)) => return Err(err),
                Err(RecurramError::InvalidData(
                    "state patch reconstruction unsupported for this message kind",
                )) => {}
                Err(err) => return Err(err),
            },
            Message::TemplateBatch { .. } => {
                if self.state.previous_message.is_none() {
                    self.state.previous_message = Some(msg.clone());
                }
            }
            _ => {
                self.state.previous_message = Some(msg.clone());
            }
        }
        Ok(msg)
    }

    pub fn encode_value(&mut self, value: &Value) -> Result<Vec<u8>> {
        let message = self.message_for_value(value);
        let bytes = self.encode_message(&message)?;
        self.state.previous_message = Some(message);
        Ok(bytes)
    }

    pub fn decode_value(&mut self, bytes: &[u8]) -> Result<Value> {
        let message = self.decode_message(bytes)?;
        self.state.previous_message = Some(message.clone());
        match message {
            Message::Scalar(value) => Ok(value),
            Message::Array(values) => Ok(Value::Array(values)),
            Message::Map(entries) => Ok(Value::Map(entries_to_map(entries, &self.state)?)),
            Message::ShapedObject {
                shape_id,
                presence,
                values,
            } => {
                let keys = self
                    .state
                    .shape_table
                    .get_keys(shape_id)
                    .ok_or_else(|| self.reference_error("shape_id", shape_id))?;
                Ok(Value::Map(shape_values_to_map(keys, presence, values)))
            }
            Message::TypedVector(vec) => Ok(typed_vector_to_value(vec)),
            _ => Err(RecurramError::InvalidData(
                "decode_value expects scalar/array/map/vector message",
            )),
        }
    }

    fn reference_error(&self, kind: &'static str, id: u64) -> RecurramError {
        match self.state.options.unknown_reference_policy {
            UnknownReferencePolicy::FailFast => RecurramError::UnknownReference(kind, id),
            UnknownReferencePolicy::StatelessRetry => RecurramError::StatelessRetryRequired(kind, id),
        }
    }

    fn message_for_value(&mut self, value: &Value) -> Message {
        match value {
            Value::Array(items) => {
                if let Some(vector) = self.try_make_typed_vector(items) {
                    Message::TypedVector(vector)
                } else {
                    Message::Array(items.clone())
                }
            }
            Value::Map(entries) => {
                let keys: Vec<String> = entries.iter().map(|(k, _)| k.clone()).collect();
                let had_observation = self.state.encode_shape_observations.contains_key(&keys);
                let encode_observed = self.observe_encode_shape_candidate(&keys);
                if let Some(shape_id) = self.state.shape_table.get_id(&keys)
                    && (!had_observation || encode_observed >= 2)
                {
                    self.shaped_message(shape_id, entries)
                } else {
                    self.map_message(entries)
                }
            }
            scalar => Message::Scalar(scalar.clone()),
        }
    }

    fn map_message(&mut self, entries: &[(String, Value)]) -> Message {
        let map_entries = entries
            .iter()
            .map(|(k, v)| {
                let key_id = self.state.key_table.get_id(k);
                let key_ref = match key_id {
                    Some(id) => KeyRef::Id(id),
                    None => {
                        self.state.key_table.register(k.clone());
                        KeyRef::Literal(k.clone())
                    }
                };
                MapEntry {
                    key: key_ref,
                    value: v.clone(),
                }
            })
            .collect();
        Message::Map(map_entries)
    }

    fn shaped_message(&mut self, shape_id: u64, entries: &[(String, Value)]) -> Message {
        let keys = self
            .state
            .shape_table
            .get_keys(shape_id)
            .map(|k| k.to_vec())
            .unwrap_or_default();
        let mut values = Vec::with_capacity(keys.len());
        let mut presence = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some((_, value)) = entries.iter().find(|(k, _)| *k == key) {
                presence.push(true);
                values.push(value.clone());
            } else {
                presence.push(false);
            }
        }
        let presence = if presence.iter().all(|v| *v) {
            None
        } else {
            Some(presence)
        };
        Message::ShapedObject {
            shape_id,
            presence,
            values,
        }
    }

    fn try_make_typed_vector(&self, values: &[Value]) -> Option<TypedVector> {
        if values.len() < 4 {
            return None;
        }
        if values.iter().all(|v| matches!(v, Value::Bool(_))) {
            let vals: Vec<bool> = values
                .iter()
                .map(|v| match v {
                    Value::Bool(b) => *b,
                    _ => unreachable!(),
                })
                .collect();
            return Some(TypedVector {
                element_type: ElementType::Bool,
                codec: VectorCodec::DirectBitpack,
                data: TypedVectorData::Bool(vals),
            });
        }
        if values.iter().all(|v| matches!(v, Value::I64(_))) {
            let vals: Vec<i64> = values
                .iter()
                .map(|v| match v {
                    Value::I64(i) => *i,
                    _ => unreachable!(),
                })
                .collect();
            return Some(TypedVector {
                element_type: ElementType::I64,
                codec: select_integer_codec(&vals),
                data: TypedVectorData::I64(vals),
            });
        }
        if values.iter().all(|v| matches!(v, Value::U64(_))) {
            let vals: Vec<u64> = values
                .iter()
                .map(|v| match v {
                    Value::U64(i) => *i,
                    _ => unreachable!(),
                })
                .collect();
            return Some(TypedVector {
                element_type: ElementType::U64,
                codec: select_u64_codec(&vals),
                data: TypedVectorData::U64(vals),
            });
        }
        if values.iter().all(|v| matches!(v, Value::F64(_))) {
            let vals: Vec<f64> = values
                .iter()
                .map(|v| match v {
                    Value::F64(f) => *f,
                    _ => unreachable!(),
                })
                .collect();
            return Some(TypedVector {
                element_type: ElementType::F64,
                codec: select_float_codec(&vals),
                data: TypedVectorData::F64(vals),
            });
        }
        if values.iter().all(|v| matches!(v, Value::String(_))) {
            let vals: Vec<String> = values
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    _ => unreachable!(),
                })
                .collect();
            return Some(TypedVector {
                element_type: ElementType::String,
                codec: select_string_codec(&vals),
                data: TypedVectorData::String(vals),
            });
        }
        None
    }

    fn write_message(&mut self, message: &Message, out: &mut Vec<u8>) -> Result<()> {
        match message {
            Message::Scalar(value) => {
                out.push(MessageKind::Scalar as u8);
                self.write_value(value, out);
            }
            Message::Array(values) => {
                out.push(MessageKind::Array as u8);
                encode_varuint(values.len() as u64, out);
                for value in values {
                    self.write_value(value, out);
                }
            }
            Message::Map(entries) => {
                out.push(MessageKind::Map as u8);
                encode_varuint(entries.len() as u64, out);
                for entry in entries {
                    self.write_key_ref(&entry.key, out);
                    let field_identity = match &entry.key {
                        KeyRef::Literal(k) => Some(k.clone()),
                        KeyRef::Id(id) => self.state.key_table.get_value(*id).map(str::to_string),
                    };
                    self.write_value_with_field(&entry.value, field_identity.as_deref(), out);
                }
            }
            Message::ShapedObject {
                shape_id,
                presence,
                values,
            } => {
                out.push(MessageKind::ShapedObject as u8);
                encode_varuint(*shape_id, out);
                self.write_presence(presence.as_deref(), out);
                encode_varuint(values.len() as u64, out);
                let keys = self
                    .state
                    .shape_table
                    .get_keys(*shape_id)
                    .map(|k| k.to_vec());
                if let Some(keys) = keys {
                    let presence_bits = presence.clone().unwrap_or_else(|| vec![true; keys.len()]);
                    let mut value_idx = 0usize;
                    for (idx, key) in keys.iter().enumerate() {
                        let present = presence_bits.get(idx).copied().unwrap_or(true);
                        if !present {
                            continue;
                        }
                        if let Some(value) = values.get(value_idx) {
                            self.write_value_with_field(value, Some(key), out);
                            value_idx += 1;
                        }
                    }
                    while value_idx < values.len() {
                        self.write_value_with_field(&values[value_idx], None, out);
                        value_idx += 1;
                    }
                } else {
                    for value in values {
                        self.write_value_with_field(value, None, out);
                    }
                }
            }
            Message::SchemaObject {
                schema_id,
                presence,
                fields,
            } => {
                out.push(MessageKind::SchemaObject as u8);
                let mut effective_schema_id = None;
                match schema_id {
                    Some(id) => {
                        out.push(1);
                        encode_varuint(*id, out);
                        effective_schema_id = Some(*id);
                    }
                    None => out.push(0),
                }
                self.write_presence(presence.as_deref(), out);
                encode_varuint(fields.len() as u64, out);
                let schema = effective_schema_id
                    .or(self.state.last_schema_id)
                    .and_then(|id| self.state.schemas.get(&id).cloned());
                if let Some(schema) = schema {
                    out.push(1);
                    self.write_schema_fields(&schema, presence.as_deref(), fields, out)?;
                    if let Some(id) = effective_schema_id.or(self.state.last_schema_id) {
                        self.state.last_schema_id = Some(id);
                    }
                } else {
                    out.push(0);
                    for field in fields {
                        self.write_value(field, out);
                    }
                }
            }
            Message::TypedVector(vector) => {
                out.push(MessageKind::TypedVector as u8);
                self.write_typed_vector(vector, out)?;
            }
            Message::RowBatch { rows } => {
                out.push(MessageKind::RowBatch as u8);
                encode_varuint(rows.len() as u64, out);
                for row in rows {
                    encode_varuint(row.len() as u64, out);
                    for value in row {
                        self.write_value(value, out);
                    }
                }
            }
            Message::ColumnBatch { count, columns } => {
                out.push(MessageKind::ColumnBatch as u8);
                encode_varuint(*count, out);
                encode_varuint(columns.len() as u64, out);
                for column in columns {
                    self.write_column(column, out)?;
                }
            }
            Message::Control(control) => {
                out.push(MessageKind::Control as u8);
                self.write_control(control, out)?;
            }
            Message::Ext { ext_type, payload } => {
                out.push(MessageKind::Ext as u8);
                encode_varuint(*ext_type, out);
                encode_bytes(payload, out);
            }
            Message::StatePatch {
                base_ref,
                operations,
                literals,
            } => {
                out.push(MessageKind::StatePatch as u8);
                self.write_base_ref(base_ref, out);
                encode_varuint(operations.len() as u64, out);
                for operation in operations {
                    encode_varuint(operation.field_id, out);
                    out.push(operation.opcode as u8);
                    match &operation.value {
                        Some(v) => {
                            out.push(1);
                            self.write_value(v, out);
                        }
                        None => out.push(0),
                    }
                }
                encode_varuint(literals.len() as u64, out);
                for value in literals {
                    self.write_value(value, out);
                }
            }
            Message::TemplateBatch {
                template_id,
                count,
                changed_column_mask,
                columns,
            } => {
                out.push(MessageKind::TemplateBatch as u8);
                encode_varuint(*template_id, out);
                encode_varuint(*count, out);
                encode_bitmap(changed_column_mask, out);
                encode_varuint(columns.len() as u64, out);
                for column in columns {
                    self.write_column(column, out)?;
                }
            }
            Message::ControlStream { codec, payload } => {
                out.push(MessageKind::ControlStream as u8);
                out.push(*codec as u8);
                self.write_control_stream_payload(*codec, payload, out);
            }
            Message::BaseSnapshot {
                base_id,
                schema_or_shape_ref,
                payload,
            } => {
                out.push(MessageKind::BaseSnapshot as u8);
                encode_varuint(*base_id, out);
                encode_varuint(*schema_or_shape_ref, out);
                self.write_message(payload, out)?;
                self.state
                    .register_base_snapshot(*base_id, payload.as_ref().clone());
            }
        }
        Ok(())
    }

    fn read_message(&mut self, reader: &mut Reader<'_>) -> Result<Message> {
        let kind_byte = reader.read_u8()?;
        let kind = MessageKind::from_byte(kind_byte).ok_or(RecurramError::InvalidKind(kind_byte))?;
        let message = match kind {
            MessageKind::Scalar => Message::Scalar(self.read_value(reader)?),
            MessageKind::Array => {
                let len = reader.read_varuint()? as usize;
                let mut values = Vec::with_capacity(len);
                for _ in 0..len {
                    values.push(self.read_value(reader)?);
                }
                Message::Array(values)
            }
            MessageKind::Map => {
                let len = reader.read_varuint()? as usize;
                let mut entries = Vec::with_capacity(len);
                for _ in 0..len {
                    let key = self.read_key_ref(reader)?;
                    let field_identity = match &key {
                        KeyRef::Literal(k) => Some(k.clone()),
                        KeyRef::Id(id) => self.state.key_table.get_value(*id).map(str::to_string),
                    };
                    let value = self.read_value_with_field(reader, field_identity.as_deref())?;
                    entries.push(MapEntry { key, value });
                }
                let keys: Vec<String> = entries
                    .iter()
                    .filter_map(|entry| match &entry.key {
                        KeyRef::Literal(k) => Some(k.clone()),
                        KeyRef::Id(id) => self.state.key_table.get_value(*id).map(str::to_string),
                    })
                    .collect();
                if keys.len() == entries.len() {
                    self.observe_decode_shape_candidate(&keys);
                }
                Message::Map(entries)
            }
            MessageKind::ShapedObject => {
                let shape_id = reader.read_varuint()?;
                let presence = self.read_presence(reader)?;
                let len = reader.read_varuint()? as usize;
                let mut values = Vec::with_capacity(len);
                if let Some(keys) = self
                    .state
                    .shape_table
                    .get_keys(shape_id)
                    .map(|k| k.to_vec())
                {
                    let presence_bits = presence.clone().unwrap_or_else(|| vec![true; keys.len()]);
                    let mut read_count = 0usize;
                    for (idx, key) in keys.iter().enumerate() {
                        let present = presence_bits.get(idx).copied().unwrap_or(true);
                        if !present {
                            continue;
                        }
                        if read_count >= len {
                            break;
                        }
                        values.push(self.read_value_with_field(reader, Some(key))?);
                        read_count += 1;
                    }
                    while read_count < len {
                        values.push(self.read_value_with_field(reader, None)?);
                        read_count += 1;
                    }
                } else {
                    for _ in 0..len {
                        values.push(self.read_value_with_field(reader, None)?);
                    }
                }
                Message::ShapedObject {
                    shape_id,
                    presence,
                    values,
                }
            }
            MessageKind::SchemaObject => {
                let has_schema = reader.read_u8()?;
                let schema_id = match has_schema {
                    0 => None,
                    1 => Some(reader.read_varuint()?),
                    _ => return Err(RecurramError::InvalidData("schema flag")),
                };
                let presence = self.read_presence(reader)?;
                let len = reader.read_varuint()? as usize;
                let encoding_mode = reader.read_u8()?;
                let mut fields = Vec::with_capacity(len);
                if encoding_mode == 1 {
                    let effective_schema_id =
                        schema_id
                            .or(self.state.last_schema_id)
                            .ok_or(RecurramError::InvalidData(
                                "schema object requires schema id in context",
                            ))?;
                    let schema = self
                        .state
                        .schemas
                        .get(&effective_schema_id)
                        .ok_or_else(|| self.reference_error("schema_id", effective_schema_id))?
                        .clone();
                    fields = self.read_schema_fields(&schema, presence.as_deref(), len, reader)?;
                    self.state.last_schema_id = Some(effective_schema_id);
                } else if encoding_mode == 0 {
                    for _ in 0..len {
                        fields.push(self.read_value(reader)?);
                    }
                    if let Some(id) = schema_id {
                        self.state.last_schema_id = Some(id);
                    }
                } else {
                    return Err(RecurramError::InvalidData("schema object encoding mode"));
                }
                Message::SchemaObject {
                    schema_id,
                    presence,
                    fields,
                }
            }
            MessageKind::TypedVector => Message::TypedVector(self.read_typed_vector(reader, None)?),
            MessageKind::RowBatch => {
                let row_count = reader.read_varuint()? as usize;
                let mut rows = Vec::with_capacity(row_count);
                for _ in 0..row_count {
                    let field_count = reader.read_varuint()? as usize;
                    let mut row = Vec::with_capacity(field_count);
                    for _ in 0..field_count {
                        row.push(self.read_value(reader)?);
                    }
                    rows.push(row);
                }
                Message::RowBatch { rows }
            }
            MessageKind::ColumnBatch => {
                let count = reader.read_varuint()?;
                let column_count = reader.read_varuint()? as usize;
                let mut columns = Vec::with_capacity(column_count);
                for _ in 0..column_count {
                    columns.push(self.read_column(reader)?);
                }
                Message::ColumnBatch { count, columns }
            }
            MessageKind::Control => Message::Control(self.read_control(reader)?),
            MessageKind::Ext => Message::Ext {
                ext_type: reader.read_varuint()?,
                payload: reader.read_bytes()?,
            },
            MessageKind::StatePatch => {
                let base_ref = self.read_base_ref(reader)?;
                let len = reader.read_varuint()? as usize;
                let mut operations = Vec::with_capacity(len);
                for _ in 0..len {
                    let field_id = reader.read_varuint()?;
                    let op_byte = reader.read_u8()?;
                    let opcode = PatchOpcode::from_byte(op_byte)
                        .ok_or(RecurramError::InvalidData("patch opcode"))?;
                    let has_value = reader.read_u8()?;
                    let value = if has_value == 1 {
                        Some(self.read_value(reader)?)
                    } else {
                        None
                    };
                    operations.push(PatchOperation {
                        field_id,
                        opcode,
                        value,
                    });
                }
                let lit_len = reader.read_varuint()? as usize;
                let mut literals = Vec::with_capacity(lit_len);
                for _ in 0..lit_len {
                    literals.push(self.read_value(reader)?);
                }
                Message::StatePatch {
                    base_ref,
                    operations,
                    literals,
                }
            }
            MessageKind::TemplateBatch => {
                let template_id = reader.read_varuint()?;
                let count = reader.read_varuint()?;
                let changed_column_mask = reader.read_bitmap()?;
                let col_len = reader.read_varuint()? as usize;
                let mut changed_columns = Vec::with_capacity(col_len);
                for _ in 0..col_len {
                    changed_columns.push(self.read_column(reader)?);
                }
                let has_template = self.state.template_columns.contains_key(&template_id);
                let full_columns = if has_template {
                    merge_template_columns(
                        self.state
                            .template_columns
                            .get(&template_id)
                            .ok_or_else(|| self.reference_error("template_id", template_id))?,
                        &changed_column_mask,
                        changed_columns.clone(),
                    )?
                } else {
                    if changed_column_mask.iter().any(|b| !*b) {
                        return Err(self.reference_error("template_id", template_id));
                    }
                    changed_columns.clone()
                };
                self.state
                    .template_columns
                    .insert(template_id, full_columns.clone());
                self.state.templates.insert(
                    template_id,
                    template_descriptor_from_columns(template_id, &full_columns),
                );
                if count >= 16 {
                    self.state.previous_message = Some(Message::ColumnBatch {
                        count,
                        columns: full_columns,
                    });
                }
                Message::TemplateBatch {
                    template_id,
                    count,
                    changed_column_mask,
                    columns: changed_columns,
                }
            }
            MessageKind::ControlStream => {
                let codec = ControlStreamCodec::from_byte(reader.read_u8()?)
                    .ok_or(RecurramError::InvalidData("control stream codec"))?;
                let payload = self.read_control_stream_payload(codec, reader)?;
                Message::ControlStream { codec, payload }
            }
            MessageKind::BaseSnapshot => {
                let base_id = reader.read_varuint()?;
                let schema_or_shape_ref = reader.read_varuint()?;
                let payload = Box::new(self.read_message(reader)?);
                self.state
                    .register_base_snapshot(base_id, payload.as_ref().clone());
                Message::BaseSnapshot {
                    base_id,
                    schema_or_shape_ref,
                    payload,
                }
            }
        };
        Ok(message)
    }

    fn write_value(&mut self, value: &Value, out: &mut Vec<u8>) {
        self.write_value_with_field(value, None, out);
    }

    fn write_value_with_field(
        &mut self,
        value: &Value,
        field_identity: Option<&str>,
        out: &mut Vec<u8>,
    ) {
        match value {
            Value::Null => out.push(TAG_NULL),
            Value::Bool(false) => out.push(TAG_BOOL_FALSE),
            Value::Bool(true) => out.push(TAG_BOOL_TRUE),
            Value::I64(v) => {
                out.push(TAG_I64);
                write_smallest_u64(encode_zigzag(*v), out);
            }
            Value::U64(v) => {
                out.push(TAG_U64);
                write_smallest_u64(*v, out);
            }
            Value::F64(v) => {
                out.push(TAG_F64);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Value::String(v) => {
                out.push(TAG_STRING);
                if let Some(identity) = field_identity
                    && let Some(enum_values) = self.state.field_enums.get(identity)
                    && let Some(code) = enum_values.iter().position(|candidate| candidate == v)
                {
                    out.push(StringMode::InlineEnum as u8);
                    encode_varuint(code as u64, out);
                    return;
                }
                let mode = if v.is_empty() {
                    StringMode::Empty
                } else if let Some(id) = self.state.string_table.get_id(v) {
                    out.push(StringMode::Ref as u8);
                    encode_varuint(id, out);
                    return;
                } else if let Some((base_id, prefix_len)) = self.best_prefix_base(v) {
                    let suffix = &v[prefix_len..];
                    self.state.string_table.register(v.clone());
                    out.push(StringMode::PrefixDelta as u8);
                    encode_varuint(base_id, out);
                    encode_varuint(prefix_len as u64, out);
                    encode_string(suffix, out);
                    return;
                } else {
                    self.state.string_table.register(v.clone());
                    StringMode::Literal
                };
                out.push(mode as u8);
                if matches!(mode, StringMode::Literal) {
                    encode_string(v, out);
                }
            }
            Value::Binary(v) => {
                out.push(TAG_BINARY);
                encode_bytes(v, out);
            }
            Value::Array(values) => {
                out.push(TAG_ARRAY);
                encode_varuint(values.len() as u64, out);
                for value in values {
                    self.write_value_with_field(value, None, out);
                }
            }
            Value::Map(entries) => {
                out.push(TAG_MAP);
                encode_varuint(entries.len() as u64, out);
                for (k, v) in entries {
                    encode_string(k, out);
                    self.write_value_with_field(v, Some(k), out);
                }
            }
        }
    }

    fn read_value(&mut self, reader: &mut Reader<'_>) -> Result<Value> {
        self.read_value_with_field(reader, None)
    }

    fn read_value_with_field(
        &mut self,
        reader: &mut Reader<'_>,
        field_identity: Option<&str>,
    ) -> Result<Value> {
        match reader.read_u8()? {
            TAG_NULL => Ok(Value::Null),
            TAG_BOOL_FALSE => Ok(Value::Bool(false)),
            TAG_BOOL_TRUE => Ok(Value::Bool(true)),
            TAG_I64 => Ok(Value::I64(decode_zigzag(read_smallest_u64(reader)?))),
            TAG_U64 => Ok(Value::U64(read_smallest_u64(reader)?)),
            TAG_F64 => {
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(reader.read_exact(8)?);
                Ok(Value::F64(f64::from_le_bytes(bytes)))
            }
            TAG_STRING => {
                let mode = StringMode::from_byte(reader.read_u8()?)
                    .ok_or(RecurramError::InvalidData("string mode"))?;
                match mode {
                    StringMode::Empty => Ok(Value::String(String::new())),
                    StringMode::Literal => {
                        let s = reader.read_string()?;
                        self.state.string_table.register(s.clone());
                        Ok(Value::String(s))
                    }
                    StringMode::Ref => {
                        let id = reader.read_varuint()?;
                        let value = self
                            .state
                            .string_table
                            .get_value(id)
                            .ok_or_else(|| self.reference_error("string_id", id))?;
                        Ok(Value::String(value.to_string()))
                    }
                    StringMode::PrefixDelta => {
                        let base_id = reader.read_varuint()?;
                        let prefix_len = reader.read_varuint()? as usize;
                        let suffix = reader.read_string()?;
                        let base = self
                            .state
                            .string_table
                            .get_value(base_id)
                            .ok_or_else(|| self.reference_error("string_id", base_id))?;
                        if prefix_len > base.len() || !base.is_char_boundary(prefix_len) {
                            return Err(RecurramError::InvalidData("prefix_delta prefix_len"));
                        }
                        let prefix = &base[..prefix_len];
                        let value = format!("{prefix}{suffix}");
                        self.state.string_table.register(value.clone());
                        Ok(Value::String(value))
                    }
                    StringMode::InlineEnum => {
                        let code = reader.read_varuint()? as usize;
                        let identity = field_identity
                            .ok_or(RecurramError::InvalidData("inline enum without field context"))?;
                        let values = self
                            .state
                            .field_enums
                            .get(identity)
                            .ok_or_else(|| self.reference_error("inline_enum_field", 0))?;
                        let value = values
                            .get(code)
                            .cloned()
                            .ok_or_else(|| self.reference_error("inline_enum_code", code as u64))?;
                        Ok(Value::String(value))
                    }
                }
            }
            TAG_BINARY => Ok(Value::Binary(reader.read_bytes()?)),
            TAG_ARRAY => {
                let len = reader.read_varuint()? as usize;
                let mut values = Vec::with_capacity(len);
                for _ in 0..len {
                    values.push(self.read_value_with_field(reader, None)?);
                }
                Ok(Value::Array(values))
            }
            TAG_MAP => {
                let len = reader.read_varuint()? as usize;
                let mut entries = Vec::with_capacity(len);
                for _ in 0..len {
                    let key = reader.read_string()?;
                    let value = self.read_value_with_field(reader, Some(&key))?;
                    entries.push((key, value));
                }
                Ok(Value::Map(entries))
            }
            other => Err(RecurramError::InvalidTag(other)),
        }
    }

    fn write_schema_fields(
        &mut self,
        schema: &Schema,
        presence: Option<&[bool]>,
        fields: &[Value],
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let indices = schema_present_field_indices(schema, presence)?;
        if indices.len() != fields.len() {
            return Err(RecurramError::InvalidData("schema field count mismatch"));
        }
        for (idx, value) in indices.into_iter().zip(fields.iter()) {
            self.write_schema_field_value(&schema.fields[idx], value, out)?;
        }
        Ok(())
    }

    fn read_schema_fields(
        &mut self,
        schema: &Schema,
        presence: Option<&[bool]>,
        expected_len: usize,
        reader: &mut Reader<'_>,
    ) -> Result<Vec<Value>> {
        let indices = schema_present_field_indices(schema, presence)?;
        if indices.len() != expected_len {
            return Err(RecurramError::InvalidData("schema field count mismatch"));
        }
        let mut out = Vec::with_capacity(expected_len);
        for idx in indices {
            out.push(self.read_schema_field_value(&schema.fields[idx], reader)?);
        }
        Ok(out)
    }

    fn write_schema_field_value(
        &mut self,
        field: &crate::model::SchemaField,
        value: &Value,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let ty = normalized_logical_type(&field.logical_type);
        match ty.as_str() {
            "bool" => match value {
                Value::Bool(v) => out.push(if *v { 1 } else { 0 }),
                _ => return Err(RecurramError::InvalidData("schema bool type mismatch")),
            },
            "u64" => {
                if let Value::U64(v) = value {
                    if let Some((min, max)) = field_u64_range(field) {
                        if *v >= min && *v <= max {
                            out.push(1);
                            let offset = *v - min;
                            let bits = range_bit_width_u64(min, max);
                            write_fixed_bits_u64(offset, bits, out)?;
                        } else {
                            out.push(0);
                            write_smallest_u64(*v, out);
                        }
                    } else {
                        out.push(0);
                        write_smallest_u64(*v, out);
                    }
                } else {
                    return Err(RecurramError::InvalidData("schema u64 type mismatch"));
                }
            }
            "i64" => {
                if let Value::I64(v) = value {
                    if let Some((min, max)) = field_i64_range(field) {
                        if *v >= min && *v <= max {
                            out.push(1);
                            let offset = (*v - min) as u64;
                            let bits = range_bit_width_i64(min, max);
                            write_fixed_bits_u64(offset, bits, out)?;
                        } else {
                            out.push(0);
                            write_smallest_u64(encode_zigzag(*v), out);
                        }
                    } else {
                        out.push(0);
                        write_smallest_u64(encode_zigzag(*v), out);
                    }
                } else {
                    return Err(RecurramError::InvalidData("schema i64 type mismatch"));
                }
            }
            "f64" => match value {
                Value::F64(v) => out.extend_from_slice(&v.to_le_bytes()),
                _ => return Err(RecurramError::InvalidData("schema f64 type mismatch")),
            },
            "string" => match value {
                Value::String(v) => {
                    if let Some(enum_values) = self.state.field_enums.get(&field.name)
                        && let Some(code) = enum_values.iter().position(|candidate| candidate == v)
                    {
                        out.push(1);
                        encode_varuint(code as u64, out);
                    } else {
                        out.push(0);
                        encode_string(v, out);
                    }
                }
                _ => return Err(RecurramError::InvalidData("schema string type mismatch")),
            },
            "binary" => match value {
                Value::Binary(v) => encode_bytes(v, out),
                _ => return Err(RecurramError::InvalidData("schema binary type mismatch")),
            },
            _ => {
                self.write_value(value, out);
            }
        }
        Ok(())
    }

    fn read_schema_field_value(
        &mut self,
        field: &crate::model::SchemaField,
        reader: &mut Reader<'_>,
    ) -> Result<Value> {
        let ty = normalized_logical_type(&field.logical_type);
        match ty.as_str() {
            "bool" => Ok(Value::Bool(match reader.read_u8()? {
                0 => false,
                1 => true,
                _ => return Err(RecurramError::InvalidData("schema bool value")),
            })),
            "u64" => {
                let mode = reader.read_u8()?;
                let value = if mode == 1 {
                    let (min, max) = field_u64_range(field)
                        .ok_or(RecurramError::InvalidData("schema u64 range mode"))?;
                    let bits = range_bit_width_u64(min, max);
                    let offset = read_fixed_bits_u64(reader, bits)?;
                    let span = max.saturating_sub(min);
                    if offset > span {
                        return Err(RecurramError::InvalidData("schema u64 range overflow"));
                    }
                    let value = min
                        .checked_add(offset)
                        .ok_or(RecurramError::InvalidData("schema u64 range overflow"))?;
                    if value > max {
                        return Err(RecurramError::InvalidData("schema u64 range overflow"));
                    }
                    value
                } else if mode == 0 {
                    read_smallest_u64(reader)?
                } else {
                    return Err(RecurramError::InvalidData("schema u64 mode"));
                };
                Ok(Value::U64(value))
            }
            "i64" => {
                let mode = reader.read_u8()?;
                let value = if mode == 1 {
                    let (min, max) = field_i64_range(field)
                        .ok_or(RecurramError::InvalidData("schema i64 range mode"))?;
                    let bits = range_bit_width_i64(min, max);
                    let offset = read_fixed_bits_u64(reader, bits)?;
                    let span = i128::from(max) - i128::from(min);
                    if i128::from(offset) > span {
                        return Err(RecurramError::InvalidData("schema i64 range overflow"));
                    }
                    let value = i128::from(min) + i128::from(offset);
                    let value = i64::try_from(value)
                        .map_err(|_| RecurramError::InvalidData("schema i64 range overflow"))?;
                    value
                } else if mode == 0 {
                    decode_zigzag(read_smallest_u64(reader)?)
                } else {
                    return Err(RecurramError::InvalidData("schema i64 mode"));
                };
                Ok(Value::I64(value))
            }
            "f64" => {
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(reader.read_exact(8)?);
                Ok(Value::F64(f64::from_le_bytes(bytes)))
            }
            "string" => {
                let mode = reader.read_u8()?;
                if mode == 0 {
                    Ok(Value::String(reader.read_string()?))
                } else if mode == 1 {
                    let code = reader.read_varuint()? as usize;
                    let values = self
                        .state
                        .field_enums
                        .get(&field.name)
                        .ok_or_else(|| self.reference_error("inline_enum_field", 0))?;
                    let value = values
                        .get(code)
                        .cloned()
                        .ok_or_else(|| self.reference_error("inline_enum_code", code as u64))?;
                    Ok(Value::String(value))
                } else {
                    Err(RecurramError::InvalidData("schema string mode"))
                }
            }
            "binary" => Ok(Value::Binary(reader.read_bytes()?)),
            _ => self.read_value(reader),
        }
    }

    fn write_key_ref(&self, key_ref: &KeyRef, out: &mut Vec<u8>) {
        match key_ref {
            KeyRef::Literal(v) => {
                out.push(0);
                encode_string(v, out);
            }
            KeyRef::Id(id) => {
                out.push(1);
                encode_varuint(*id, out);
            }
        }
    }

    fn read_key_ref(&mut self, reader: &mut Reader<'_>) -> Result<KeyRef> {
        let mode = reader.read_u8()?;
        match mode {
            0 => {
                let key = reader.read_string()?;
                self.state.key_table.register(key.clone());
                Ok(KeyRef::Literal(key))
            }
            1 => Ok(KeyRef::Id(reader.read_varuint()?)),
            _ => Err(RecurramError::InvalidData("key ref mode")),
        }
    }

    fn write_presence(&self, presence: Option<&[bool]>, out: &mut Vec<u8>) {
        match presence {
            None => out.push(0),
            Some(bits) => {
                let present = bits.iter().filter(|bit| **bit).count();
                let absent = bits.len().saturating_sub(present);
                if absent < present {
                    out.push(2);
                    let inverted: Vec<bool> = bits.iter().map(|bit| !*bit).collect();
                    encode_bitmap(&inverted, out);
                } else {
                    out.push(1);
                    encode_bitmap(bits, out);
                }
            }
        }
    }

    fn read_presence(&self, reader: &mut Reader<'_>) -> Result<Option<Vec<bool>>> {
        let has = reader.read_u8()?;
        match has {
            0 => Ok(None),
            1 => Ok(Some(reader.read_bitmap()?)),
            2 => Ok(Some(
                reader.read_bitmap()?.into_iter().map(|bit| !bit).collect(),
            )),
            _ => Err(RecurramError::InvalidData("presence flag")),
        }
    }

    fn write_typed_vector(&mut self, vector: &TypedVector, out: &mut Vec<u8>) -> Result<()> {
        out.push(vector.element_type as u8);
        encode_varuint(typed_vector_len(&vector.data) as u64, out);
        out.push(vector.codec as u8);
        match &vector.data {
            TypedVectorData::Bool(values) => encode_bitmap(values, out),
            TypedVectorData::I64(values) => encode_i64_vector(values, vector.codec, out),
            TypedVectorData::U64(values) => encode_u64_vector(values, vector.codec, out),
            TypedVectorData::F64(values) => encode_f64_vector(values, vector.codec, out),
            TypedVectorData::String(values) => {
                self.write_string_vector(values, vector.codec, out);
            }
            TypedVectorData::Binary(values) => {
                encode_varuint(values.len() as u64, out);
                for value in values {
                    encode_bytes(value, out);
                }
            }
            TypedVectorData::Value(values) => {
                encode_varuint(values.len() as u64, out);
                for value in values {
                    self.write_value(value, out);
                }
            }
        }
        Ok(())
    }

    fn read_typed_vector(
        &mut self,
        reader: &mut Reader<'_>,
        expected_codec: Option<VectorCodec>,
    ) -> Result<TypedVector> {
        let element_type = ElementType::from_byte(reader.read_u8()?)
            .ok_or(RecurramError::InvalidData("element type"))?;
        let expected_len = reader.read_varuint()? as usize;
        let codec = VectorCodec::from_byte(reader.read_u8()?)
            .ok_or(RecurramError::InvalidData("vector codec"))?;
        if let Some(expected) = expected_codec
            && codec != expected
        {
            return Err(RecurramError::InvalidData("column codec mismatch"));
        }
        let data = match element_type {
            ElementType::Bool => TypedVectorData::Bool(reader.read_bitmap()?),
            ElementType::I64 => TypedVectorData::I64(decode_i64_vector(reader, codec)?),
            ElementType::U64 => TypedVectorData::U64(decode_u64_vector(reader, codec)?),
            ElementType::F64 => TypedVectorData::F64(decode_f64_vector(reader, codec)?),
            ElementType::String => TypedVectorData::String(self.read_string_vector(reader, codec)?),
            ElementType::Binary => {
                let len = reader.read_varuint()? as usize;
                let mut values = Vec::with_capacity(len);
                for _ in 0..len {
                    values.push(reader.read_bytes()?);
                }
                TypedVectorData::Binary(values)
            }
            ElementType::Value => {
                let len = reader.read_varuint()? as usize;
                let mut values = Vec::with_capacity(len);
                for _ in 0..len {
                    values.push(self.read_value(reader)?);
                }
                TypedVectorData::Value(values)
            }
        };
        if typed_vector_len(&data) != expected_len {
            return Err(RecurramError::InvalidData("typed vector length mismatch"));
        }
        Ok(TypedVector {
            element_type,
            codec,
            data,
        })
    }

    fn write_column(&mut self, column: &Column, out: &mut Vec<u8>) -> Result<()> {
        encode_varuint(column.field_id, out);
        out.push(column.null_strategy as u8);
        match column.null_strategy {
            NullStrategy::PresenceBitmap | NullStrategy::InvertedPresenceBitmap => {
                let presence = column
                    .presence
                    .as_deref()
                    .ok_or(RecurramError::InvalidData("missing column presence bitmap"))?;
                encode_bitmap(presence, out);
            }
            NullStrategy::None | NullStrategy::AllPresentElided => {}
        }
        out.push(column.codec as u8);
        match column.dictionary_id {
            Some(id) => {
                out.push(1);
                encode_varuint(id, out);
                if let (Some(payload), Some(profile)) = (
                    self.state.dictionaries.get(&id),
                    self.state.dictionary_profiles.get(&id),
                ) {
                    out.push(1);
                    encode_varuint(profile.version, out);
                    encode_varuint(profile.hash, out);
                    encode_varuint(profile.expires_at, out);
                    out.push(profile.fallback as u8);
                    encode_bytes(payload, out);
                } else {
                    out.push(0);
                }
            }
            None => out.push(0),
        }
        let trained_block = if let (Some(dict_id), TypedVectorData::String(values)) =
            (column.dictionary_id, &column.values)
        {
            if matches!(
                column.codec,
                VectorCodec::Dictionary | VectorCodec::StringRef
            ) {
                if let Some(payload) = self.state.dictionaries.get(&dict_id) {
                    let dictionary = decode_trained_dictionary_payload(payload)?;
                    encode_trained_dictionary_block(values, &dictionary)?
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some(block) = trained_block {
            out.push(1);
            encode_bytes(&block, out);
            return Ok(());
        }
        out.push(0);
        let element_type = match &column.values {
            TypedVectorData::Bool(_) => ElementType::Bool,
            TypedVectorData::I64(_) => ElementType::I64,
            TypedVectorData::U64(_) => ElementType::U64,
            TypedVectorData::F64(_) => ElementType::F64,
            TypedVectorData::String(_) => ElementType::String,
            TypedVectorData::Binary(_) => ElementType::Binary,
            TypedVectorData::Value(_) => ElementType::Value,
        };
        let vector = TypedVector {
            element_type,
            codec: column.codec,
            data: column.values.clone(),
        };
        self.write_typed_vector(&vector, out)
    }

    fn read_column(&mut self, reader: &mut Reader<'_>) -> Result<Column> {
        let field_id = reader.read_varuint()?;
        let null_strategy = NullStrategy::from_byte(reader.read_u8()?)
            .ok_or(RecurramError::InvalidData("null strategy"))?;
        let presence = match null_strategy {
            NullStrategy::PresenceBitmap | NullStrategy::InvertedPresenceBitmap => {
                Some(reader.read_bitmap()?)
            }
            NullStrategy::None | NullStrategy::AllPresentElided => None,
        };
        let codec =
            VectorCodec::from_byte(reader.read_u8()?).ok_or(RecurramError::InvalidData("codec"))?;
        let has_dict = reader.read_u8()?;
        let dictionary_id = match has_dict {
            0 => None,
            1 => {
                let id = reader.read_varuint()?;
                let has_profile = reader.read_u8()?;
                match has_profile {
                    0 => {
                        if !self.state.dictionaries.contains_key(&id) {
                            return Err(self.reference_error("dict_id", id));
                        }
                    }
                    1 => {
                        let version = reader.read_varuint()?;
                        let hash = reader.read_varuint()?;
                        let expires_at = reader.read_varuint()?;
                        let fallback = DictionaryFallback::from_byte(reader.read_u8()?)
                            .ok_or(RecurramError::InvalidData("dictionary fallback"))?;
                        let payload = reader.read_bytes()?;
                        if dictionary_payload_hash(&payload) != hash {
                            return Err(RecurramError::InvalidData("dictionary profile hash mismatch"));
                        }
                        self.state.dictionaries.insert(id, payload);
                        self.state.dictionary_profiles.insert(
                            id,
                            DictionaryProfile {
                                version,
                                hash,
                                expires_at,
                                fallback,
                            },
                        );
                    }
                    _ => return Err(RecurramError::InvalidData("dictionary profile flag")),
                }
                Some(id)
            }
            _ => return Err(RecurramError::InvalidData("dictionary flag")),
        };
        let payload_mode = reader.read_u8()?;
        let values = if payload_mode == 0 {
            self.read_typed_vector(reader, Some(codec))?.data
        } else if payload_mode == 1 {
            let dict_id = dictionary_id.ok_or(RecurramError::InvalidData(
                "trained dictionary block requires dict_id",
            ))?;
            if !matches!(codec, VectorCodec::Dictionary | VectorCodec::StringRef) {
                return Err(RecurramError::InvalidData(
                    "trained dictionary block requires string dictionary codec",
                ));
            }
            let dictionary_payload = self
                .state
                .dictionaries
                .get(&dict_id)
                .ok_or_else(|| self.reference_error("dict_id", dict_id))?;
            let dictionary = decode_trained_dictionary_payload(dictionary_payload)?;
            let block = reader.read_bytes()?;
            let values = decode_trained_dictionary_block(&block, &dictionary)?;
            TypedVectorData::String(values)
        } else {
            return Err(RecurramError::InvalidData("column payload mode"));
        };
        Ok(Column {
            field_id,
            null_strategy,
            presence,
            codec,
            dictionary_id,
            values,
        })
    }

    fn write_control(&mut self, control: &ControlMessage, out: &mut Vec<u8>) -> Result<()> {
        match control {
            ControlMessage::RegisterKeys(keys) => {
                out.push(ControlOpcode::RegisterKeys as u8);
                encode_varuint(keys.len() as u64, out);
                for key in keys {
                    self.state.key_table.register(key.clone());
                    encode_string(key, out);
                }
            }
            ControlMessage::RegisterShape { shape_id, keys } => {
                out.push(ControlOpcode::RegisterShape as u8);
                encode_varuint(*shape_id, out);
                encode_varuint(keys.len() as u64, out);
                let mut literals = Vec::new();
                for key in keys {
                    self.write_key_ref(key, out);
                    let literal = match key {
                        KeyRef::Literal(v) => v.clone(),
                        KeyRef::Id(id) => self
                            .state
                            .key_table
                            .get_value(*id)
                            .ok_or_else(|| self.reference_error("key_id", *id))?
                            .to_string(),
                    };
                    literals.push(literal);
                }
                if !self
                    .state
                    .shape_table
                    .register_with_id(*shape_id, literals.clone())
                {
                    return Err(RecurramError::InvalidData("shape id mismatch"));
                }
                self.state.encode_shape_observations.insert(literals, 2);
            }
            ControlMessage::RegisterStrings(strings) => {
                out.push(ControlOpcode::RegisterStrings as u8);
                encode_varuint(strings.len() as u64, out);
                for value in strings {
                    self.state.string_table.register(value.clone());
                    encode_string(value, out);
                }
            }
            ControlMessage::PromoteStringFieldToEnum {
                field_identity,
                values,
            } => {
                out.push(ControlOpcode::PromoteStringFieldToEnum as u8);
                encode_string(field_identity, out);
                encode_varuint(values.len() as u64, out);
                for value in values {
                    encode_string(value, out);
                }
                self.state
                    .field_enums
                    .insert(field_identity.clone(), values.clone());
            }
            ControlMessage::ResetTables => {
                out.push(ControlOpcode::ResetTables as u8);
                self.state.reset_tables();
            }
            ControlMessage::ResetState => {
                out.push(ControlOpcode::ResetState as u8);
                self.state.reset_state();
            }
        }
        Ok(())
    }

    fn read_control(&mut self, reader: &mut Reader<'_>) -> Result<ControlMessage> {
        let op = ControlOpcode::from_byte(reader.read_u8()?)
            .ok_or(RecurramError::InvalidData("control opcode"))?;
        match op {
            ControlOpcode::RegisterKeys => {
                let len = reader.read_varuint()? as usize;
                let mut keys = Vec::with_capacity(len);
                for _ in 0..len {
                    let key = reader.read_string()?;
                    self.state.key_table.register(key.clone());
                    keys.push(key);
                }
                Ok(ControlMessage::RegisterKeys(keys))
            }
            ControlOpcode::RegisterShape => {
                let shape_id = reader.read_varuint()?;
                let len = reader.read_varuint()? as usize;
                let mut key_refs = Vec::with_capacity(len);
                let mut keys = Vec::with_capacity(len);
                for _ in 0..len {
                    let key_ref = self.read_key_ref(reader)?;
                    let key = match &key_ref {
                        KeyRef::Literal(v) => v.clone(),
                        KeyRef::Id(id) => self
                            .state
                            .key_table
                            .get_value(*id)
                            .ok_or_else(|| self.reference_error("key_id", *id))?
                            .to_string(),
                    };
                    keys.push(key);
                    key_refs.push(key_ref);
                }
                if !self.state.shape_table.register_with_id(shape_id, keys) {
                    return Err(RecurramError::InvalidData("shape id mismatch"));
                }
                Ok(ControlMessage::RegisterShape {
                    shape_id,
                    keys: key_refs,
                })
            }
            ControlOpcode::RegisterStrings => {
                let len = reader.read_varuint()? as usize;
                let mut values = Vec::with_capacity(len);
                for _ in 0..len {
                    let value = reader.read_string()?;
                    self.state.string_table.register(value.clone());
                    values.push(value);
                }
                Ok(ControlMessage::RegisterStrings(values))
            }
            ControlOpcode::PromoteStringFieldToEnum => {
                let field_identity = reader.read_string()?;
                let len = reader.read_varuint()? as usize;
                let mut values = Vec::with_capacity(len);
                for _ in 0..len {
                    values.push(reader.read_string()?);
                }
                self.state
                    .field_enums
                    .insert(field_identity.clone(), values.clone());
                Ok(ControlMessage::PromoteStringFieldToEnum {
                    field_identity,
                    values,
                })
            }
            ControlOpcode::ResetTables => {
                self.state.reset_tables();
                Ok(ControlMessage::ResetTables)
            }
            ControlOpcode::ResetState => {
                self.state.reset_state();
                Ok(ControlMessage::ResetState)
            }
        }
    }

    fn write_base_ref(&self, base_ref: &BaseRef, out: &mut Vec<u8>) {
        match base_ref {
            BaseRef::Previous => out.push(0),
            BaseRef::BaseId(id) => {
                out.push(1);
                encode_varuint(*id, out);
            }
        }
    }

    fn read_base_ref(&self, reader: &mut Reader<'_>) -> Result<BaseRef> {
        match reader.read_u8()? {
            0 => {
                if self.state.previous_message.is_none() {
                    return Err(self.reference_error("previous_message", 0));
                }
                Ok(BaseRef::Previous)
            }
            1 => {
                let id = reader.read_varuint()?;
                if self.state.get_base_snapshot(id).is_none() {
                    return Err(self.reference_error("base_id", id));
                }
                Ok(BaseRef::BaseId(id))
            }
            _ => Err(RecurramError::InvalidData("base_ref kind")),
        }
    }

    fn write_control_stream_payload(
        &self,
        codec: ControlStreamCodec,
        payload: &[u8],
        out: &mut Vec<u8>,
    ) {
        match codec {
            ControlStreamCodec::Plain => encode_bytes(payload, out),
            ControlStreamCodec::Rle => {
                let encoded = rle_encode_bytes(payload);
                encode_bytes(&encoded, out);
            }
            ControlStreamCodec::Bitpack => {
                let encoded = control_bitpack_encode_bytes(payload);
                encode_bytes(&encoded, out);
            }
            ControlStreamCodec::Huffman => {
                let encoded = control_huffman_encode_bytes(payload);
                encode_bytes(&encoded, out);
            }
            ControlStreamCodec::Fse => {
                let encoded = control_fse_encode_bytes(payload);
                encode_bytes(&encoded, out);
            }
        }
    }

    fn read_control_stream_payload(
        &self,
        codec: ControlStreamCodec,
        reader: &mut Reader<'_>,
    ) -> Result<Vec<u8>> {
        let encoded = reader.read_bytes()?;
        match codec {
            ControlStreamCodec::Plain => Ok(encoded),
            ControlStreamCodec::Rle => rle_decode_bytes(&encoded),
            ControlStreamCodec::Bitpack => control_bitpack_decode_bytes(&encoded),
            ControlStreamCodec::Huffman => control_huffman_decode_bytes(&encoded),
            ControlStreamCodec::Fse => control_fse_decode_bytes(&encoded),
        }
    }

    fn best_prefix_base(&self, value: &str) -> Option<(u64, usize)> {
        let bytes = value.as_bytes();
        let mut best: Option<(u64, usize)> = None;
        for (idx, candidate) in self.state.string_table.by_id.iter().enumerate() {
            let prefix_len = common_prefix_len(bytes, candidate.as_bytes());
            if prefix_len < 3 || !value.is_char_boundary(prefix_len) {
                continue;
            }
            let suffix_len = bytes.len().saturating_sub(prefix_len);
            let literal_cost = bytes.len();
            let pd_cost = suffix_len + 2;
            if pd_cost >= literal_cost {
                continue;
            }
            match best {
                Some((_, best_len)) if prefix_len <= best_len => {}
                _ => best = Some((idx as u64, prefix_len)),
            }
        }
        best
    }

    fn write_string_vector(&mut self, values: &[String], codec: VectorCodec, out: &mut Vec<u8>) {
        match codec {
            VectorCodec::Dictionary | VectorCodec::StringRef => {
                let mut dict = Vec::<String>::new();
                let mut by_value = std::collections::BTreeMap::<String, u64>::new();
                let mut ids = Vec::with_capacity(values.len());
                for value in values {
                    let id = if let Some(id) = by_value.get(value) {
                        *id
                    } else {
                        let id = dict.len() as u64;
                        by_value.insert(value.clone(), id);
                        dict.push(value.clone());
                        id
                    };
                    ids.push(id);
                }
                encode_varuint(dict.len() as u64, out);
                for value in &dict {
                    encode_string(value, out);
                }
                encode_varuint(ids.len() as u64, out);
                for id in ids {
                    encode_varuint(id, out);
                }
            }
            VectorCodec::PrefixDelta => {
                encode_varuint(values.len() as u64, out);
                if values.is_empty() {
                    return;
                }
                encode_string(&values[0], out);
                for idx in 1..values.len() {
                    let prev = &values[idx - 1];
                    let current = &values[idx];
                    let mut prefix_len = common_prefix_len(prev.as_bytes(), current.as_bytes());
                    while prefix_len > 0 && !current.is_char_boundary(prefix_len) {
                        prefix_len -= 1;
                    }
                    encode_varuint(prefix_len as u64, out);
                    encode_string(&current[prefix_len..], out);
                }
            }
            _ => {
                encode_varuint(values.len() as u64, out);
                for value in values {
                    encode_string(value, out);
                }
            }
        }
    }

    fn read_string_vector(
        &mut self,
        reader: &mut Reader<'_>,
        codec: VectorCodec,
    ) -> Result<Vec<String>> {
        match codec {
            VectorCodec::Dictionary | VectorCodec::StringRef => {
                let dict_len = reader.read_varuint()? as usize;
                let mut dict = Vec::with_capacity(dict_len);
                for _ in 0..dict_len {
                    dict.push(reader.read_string()?);
                }
                let len = reader.read_varuint()? as usize;
                let mut values = Vec::with_capacity(len);
                for _ in 0..len {
                    let id = reader.read_varuint()? as usize;
                    let value = dict
                        .get(id)
                        .ok_or(RecurramError::InvalidData("string vector dictionary id"))?
                        .clone();
                    values.push(value);
                }
                Ok(values)
            }
            VectorCodec::PrefixDelta => {
                let len = reader.read_varuint()? as usize;
                if len == 0 {
                    return Ok(Vec::new());
                }
                let first = reader.read_string()?;
                let mut values = Vec::with_capacity(len);
                values.push(first);
                for idx in 1..len {
                    let prefix_len = reader.read_varuint()? as usize;
                    let suffix = reader.read_string()?;
                    let prev = &values[idx - 1];
                    if prefix_len > prev.len() || !prev.is_char_boundary(prefix_len) {
                        return Err(RecurramError::InvalidData("prefix_delta prefix_len"));
                    }
                    let combined = format!("{}{}", &prev[..prefix_len], suffix);
                    values.push(combined);
                }
                Ok(values)
            }
            _ => {
                let len = reader.read_varuint()? as usize;
                let mut values = Vec::with_capacity(len);
                for _ in 0..len {
                    values.push(reader.read_string()?);
                }
                Ok(values)
            }
        }
    }

    fn apply_state_patch(
        &self,
        base_ref: &BaseRef,
        operations: &[PatchOperation],
        literals: &[Value],
    ) -> Result<Message> {
        let base = match base_ref {
            BaseRef::Previous => self
                .state
                .previous_message
                .as_ref()
                .ok_or_else(|| self.reference_error("previous_message", 0))?,
            BaseRef::BaseId(id) => self
                .state
                .get_base_snapshot(*id)
                .ok_or_else(|| self.reference_error("base_id", *id))?,
        };
        if let Message::Map(entries) = base {
            return apply_state_patch_map(entries, operations, literals);
        }
        let mut fields = message_fields(base);
        let mut literal_iter = literals.iter().cloned();
        for operation in operations {
            let idx = operation.field_id as usize;
            match operation.opcode {
                PatchOpcode::Keep => {}
                PatchOpcode::ReplaceScalar | PatchOpcode::ReplaceVector => {
                    let value = operation
                        .value
                        .clone()
                        .or_else(|| literal_iter.next())
                        .ok_or(RecurramError::InvalidData("patch replace missing value"))?;
                    if idx >= fields.len() {
                        return Err(RecurramError::InvalidData("patch field out of bounds"));
                    }
                    fields[idx] = value;
                }
                PatchOpcode::InsertField => {
                    let value = operation
                        .value
                        .clone()
                        .or_else(|| literal_iter.next())
                        .ok_or(RecurramError::InvalidData("patch insert missing value"))?;
                    if idx > fields.len() {
                        return Err(RecurramError::InvalidData("patch insert out of bounds"));
                    }
                    fields.insert(idx, value);
                }
                PatchOpcode::DeleteField => {
                    if idx >= fields.len() {
                        return Err(RecurramError::InvalidData("patch delete out of bounds"));
                    }
                    fields.remove(idx);
                }
                PatchOpcode::AppendVector => {
                    let value = operation
                        .value
                        .clone()
                        .or_else(|| literal_iter.next())
                        .ok_or(RecurramError::InvalidData("patch append missing value"))?;
                    if idx >= fields.len() {
                        return Err(RecurramError::InvalidData("patch append out of bounds"));
                    }
                    match (&mut fields[idx], value) {
                        (Value::Array(dst), Value::Array(mut src)) => dst.append(&mut src),
                        _ => return Err(RecurramError::InvalidData("patch append type mismatch")),
                    }
                }
                PatchOpcode::TruncateVector => {
                    let value = operation
                        .value
                        .clone()
                        .or_else(|| literal_iter.next())
                        .ok_or(RecurramError::InvalidData("patch truncate missing value"))?;
                    if idx >= fields.len() {
                        return Err(RecurramError::InvalidData("patch truncate out of bounds"));
                    }
                    let keep = match value {
                        Value::U64(v) => v as usize,
                        Value::I64(v) if v >= 0 => v as usize,
                        _ => return Err(RecurramError::InvalidData("patch truncate count")),
                    };
                    match &mut fields[idx] {
                        Value::Array(dst) => dst.truncate(keep),
                        _ => return Err(RecurramError::InvalidData("patch truncate type mismatch")),
                    }
                }
                PatchOpcode::StringRef | PatchOpcode::PrefixDelta => {
                    let value = operation
                        .value
                        .clone()
                        .or_else(|| literal_iter.next())
                        .ok_or(RecurramError::InvalidData("patch string op missing value"))?;
                    if idx >= fields.len() {
                        return Err(RecurramError::InvalidData("patch field out of bounds"));
                    }
                    fields[idx] = value;
                }
            }
        }
        rebuild_message_like(base, fields)
    }

    fn observe_decode_shape_candidate(&mut self, keys: &[String]) {
        if self.state.shape_table.get_id(keys).is_some() {
            return;
        }
        let observed = self.state.shape_table.observe(keys);
        if should_register_shape(keys, observed) {
            self.state.shape_table.register(keys.to_vec());
        }
    }

    fn observe_encode_shape_candidate(&mut self, keys: &[String]) -> u64 {
        let observed = *self
            .state
            .encode_shape_observations
            .entry(keys.to_vec())
            .and_modify(|count| *count += 1)
            .or_insert(1);
        if self.state.shape_table.get_id(keys).is_none() && should_register_shape(keys, observed) {
            self.state.shape_table.register(keys.to_vec());
        }
        observed
    }
}

#[derive(Debug, Clone)]
pub struct SessionEncoder {
    codec: RecurramCodec,
}

impl SessionEncoder {
    pub fn new(options: SessionOptions) -> Self {
        Self {
            codec: RecurramCodec::with_options(options),
        }
    }

    pub fn encode(&mut self, value: &Value) -> Result<Vec<u8>> {
        let bytes = self.codec.encode_value(value)?;
        self.record_full_message_as_base();
        Ok(bytes)
    }

    pub fn encode_with_schema(&mut self, schema: &Schema, value: &Value) -> Result<Vec<u8>> {
        let previous_schema_id = self.codec.state.last_schema_id;
        self.codec
            .state
            .schemas
            .insert(schema.schema_id, schema.clone());
        let mut fields = Vec::new();
        let mut optional_presence = Vec::new();
        let mut has_optional = false;
        for field in &schema.fields {
            let field_value = lookup_map_field(value, &field.name);
            if field.required {
                match field_value {
                    Some(v) => fields.push(v),
                    None => {
                        if let Some(default) = &field.default_value {
                            fields.push(default.clone());
                        } else {
                            return Err(RecurramError::InvalidData("missing required schema field"));
                        }
                    }
                }
            } else {
                has_optional = true;
                match field_value {
                    Some(v) => {
                        optional_presence.push(true);
                        fields.push(v);
                    }
                    None => optional_presence.push(false),
                }
            }
        }
        let presence = if has_optional && optional_presence.iter().any(|present| !present) {
            Some(optional_presence)
        } else {
            None
        };
        let omit_schema_id = previous_schema_id == Some(schema.schema_id);
        let message = Message::SchemaObject {
            schema_id: if omit_schema_id {
                None
            } else {
                Some(schema.schema_id)
            },
            presence,
            fields,
        };
        let bytes = self.codec.encode_message(&message)?;
        self.codec.state.last_schema_id = Some(schema.schema_id);
        self.codec.state.previous_message = Some(message);
        self.record_full_message_as_base();
        Ok(bytes)
    }

    pub fn encode_batch(&mut self, values: &[Value]) -> Result<Vec<u8>> {
        let rows = rows_from_values(values);
        let message = if values.len() >= 16 {
            let mut columns = rows_to_columns(&rows);
            if self.codec.state.options.enable_trained_dictionary {
                apply_dictionary_references(&mut self.codec.state, &mut columns);
            }
            Message::ColumnBatch {
                count: values.len() as u64,
                columns,
            }
        } else {
            Message::RowBatch { rows }
        };
        let bytes = self.codec.encode_message(&message)?;
        self.codec.state.previous_message = Some(message);
        self.record_full_message_as_base();
        Ok(bytes)
    }

    pub fn encode_patch(&mut self, value: &Value) -> Result<Vec<u8>> {
        if !self.codec.state.options.enable_state_patch {
            return self.encode(value);
        }
        let Some(prev) = self.codec.state.previous_message.clone() else {
            return self.encode(value);
        };
        let current_msg = match value {
            Value::Map(entries) => self.codec.map_message(entries),
            _ => self.codec.message_for_value(value),
        };
        if !supports_state_patch(&prev, &current_msg) {
            let bytes = self.codec.encode_value(value)?;
            self.record_full_message_as_base();
            return Ok(bytes);
        }
        let (ops, changed) = diff_message(&prev, &current_msg);
        let total_fields = message_fields(&prev)
            .len()
            .max(message_fields(&current_msg).len())
            .max(1);
        let prev_size = encoded_size(&prev);
        let patch_size = estimated_patch_size_with_base(BaseRef::Previous, &ops);
        let patch_ratio = changed as f64 / total_fields as f64;
        if patch_ratio <= 0.10 && patch_size < prev_size {
            let patch = Message::StatePatch {
                base_ref: BaseRef::Previous,
                operations: ops,
                literals: Vec::new(),
            };
            let bytes = self.codec.encode_message(&patch)?;
            self.codec.state.previous_message = Some(current_msg);
            return Ok(bytes);
        }

        let bytes = self.codec.encode_value(value)?;
        self.record_full_message_as_base();
        Ok(bytes)
    }

    pub fn encode_micro_batch(&mut self, values: &[Value]) -> Result<Vec<u8>> {
        if values.len() < 4 || !self.codec.state.options.enable_template_batch {
            return self.encode_batch(values);
        }
        if !has_uniform_micro_batch_shape(values) {
            return self.encode_batch(values);
        }
        let rows = rows_from_values(values);
        let mut columns = rows_to_columns(&rows);
        if self.codec.state.options.enable_trained_dictionary {
            apply_dictionary_references(&mut self.codec.state, &mut columns);
        }
        let descriptor = template_descriptor_from_columns(0, &columns);
        let template_id = find_template_id(&self.codec.state.templates, &descriptor)
            .unwrap_or_else(|| self.codec.state.allocate_template_id());
        let (changed_column_mask, changed_columns) =
            if let Some(previous) = self.codec.state.template_columns.get(&template_id) {
                diff_template_columns(previous, &columns)
            } else {
                (vec![true; columns.len()], columns.clone())
            };

        self.codec.state.templates.insert(
            template_id,
            template_descriptor_from_columns(template_id, &columns),
        );
        self.codec
            .state
            .template_columns
            .insert(template_id, columns.clone());

        let message = Message::TemplateBatch {
            template_id,
            count: values.len() as u64,
            changed_column_mask,
            columns: changed_columns,
        };
        let bytes = self.codec.encode_message(&message)?;
        self.codec.state.previous_message = Some(Message::ColumnBatch {
            count: values.len() as u64,
            columns,
        });
        Ok(bytes)
    }

    pub fn reset(&mut self) {
        self.codec.state.reset_state();
    }

    pub fn decode_message(&mut self, bytes: &[u8]) -> Result<Message> {
        self.codec.decode_message(bytes)
    }

    fn record_full_message_as_base(&mut self) {
        if self.codec.state.options.max_base_snapshots == 0 {
            return;
        }
        let Some(message) = self.codec.state.previous_message.clone() else {
            return;
        };
        let base_id = self.codec.state.allocate_base_id();
        self.codec.state.register_base_snapshot(base_id, message);
    }
}

fn lookup_map_field(value: &Value, key: &str) -> Option<Value> {
    if let Value::Map(entries) = value {
        return entries
            .iter()
            .find_map(|(k, v)| if k == key { Some(v.clone()) } else { None });
    }
    None
}

fn schema_present_field_indices(schema: &Schema, presence: Option<&[bool]>) -> Result<Vec<usize>> {
    let optional_total = schema.fields.iter().filter(|f| !f.required).count();
    if let Some(bits) = presence
        && bits.len() != optional_total
    {
        return Err(RecurramError::InvalidData("schema optional presence length"));
    }
    let mut indices = Vec::new();
    let mut optional_idx = 0usize;
    for (idx, field) in schema.fields.iter().enumerate() {
        if field.required {
            indices.push(idx);
        } else {
            let is_present = presence
                .and_then(|bits| bits.get(optional_idx).copied())
                .unwrap_or(true);
            optional_idx += 1;
            if is_present {
                indices.push(idx);
            }
        }
    }
    Ok(indices)
}

fn normalized_logical_type(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn field_u64_range(field: &crate::model::SchemaField) -> Option<(u64, u64)> {
    let min = field.min?;
    let max = field.max?;
    if min < 0 || max < min {
        return None;
    }
    Some((min as u64, max as u64))
}

fn field_i64_range(field: &crate::model::SchemaField) -> Option<(i64, i64)> {
    let min = field.min?;
    let max = field.max?;
    if max < min {
        return None;
    }
    Some((min, max))
}

fn rows_from_values(values: &[Value]) -> Vec<Vec<Value>> {
    let all_maps = values.iter().all(|v| matches!(v, Value::Map(_)));
    if !all_maps {
        return values.iter().map(|v| vec![v.clone()]).collect();
    }

    let mut key_order = Vec::<String>::new();
    for value in values {
        if let Value::Map(entries) = value {
            for (key, _) in entries {
                if !key_order.iter().any(|k| k == key) {
                    key_order.push(key.clone());
                }
            }
        }
    }

    values
        .iter()
        .map(|value| {
            let mut row = Vec::with_capacity(key_order.len());
            if let Value::Map(entries) = value {
                for key in &key_order {
                    let value = entries
                        .iter()
                        .find_map(|(k, v)| if k == key { Some(v.clone()) } else { None })
                        .unwrap_or(Value::Null);
                    row.push(value);
                }
            }
            row
        })
        .collect()
}

fn has_uniform_micro_batch_shape(values: &[Value]) -> bool {
    if values.is_empty() {
        return true;
    }
    match &values[0] {
        Value::Map(first_entries) => {
            let first_keys: Vec<&str> = first_entries.iter().map(|(k, _)| k.as_str()).collect();
            values.iter().all(|value| {
                if let Value::Map(entries) = value {
                    let keys: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
                    keys == first_keys
                } else {
                    false
                }
            })
        }
        Value::Array(_) => values.iter().all(|value| matches!(value, Value::Array(_))),
        Value::Null => values.iter().all(|value| matches!(value, Value::Null)),
        Value::Bool(_) => values.iter().all(|value| matches!(value, Value::Bool(_))),
        Value::I64(_) => values.iter().all(|value| matches!(value, Value::I64(_))),
        Value::U64(_) => values.iter().all(|value| matches!(value, Value::U64(_))),
        Value::F64(_) => values.iter().all(|value| matches!(value, Value::F64(_))),
        Value::String(_) => values.iter().all(|value| matches!(value, Value::String(_))),
        Value::Binary(_) => values.iter().all(|value| matches!(value, Value::Binary(_))),
    }
}

fn should_register_shape(keys: &[String], observed_count: u64) -> bool {
    keys.len() >= 3 && observed_count >= 2
}

fn typed_vector_len(data: &TypedVectorData) -> usize {
    match data {
        TypedVectorData::Bool(v) => v.len(),
        TypedVectorData::I64(v) => v.len(),
        TypedVectorData::U64(v) => v.len(),
        TypedVectorData::F64(v) => v.len(),
        TypedVectorData::String(v) => v.len(),
        TypedVectorData::Binary(v) => v.len(),
        TypedVectorData::Value(v) => v.len(),
    }
}

fn supports_state_patch(base: &Message, current: &Message) -> bool {
    match (base, current) {
        (Message::Scalar(_), Message::Scalar(_)) => true,
        (Message::Array(_), Message::Array(_)) => true,
        (Message::Map(a), Message::Map(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.key == y.key)
        }
        (Message::ShapedObject { shape_id: a, .. }, Message::ShapedObject { shape_id: b, .. }) => {
            a == b
        }
        (
            Message::SchemaObject { schema_id: a, .. },
            Message::SchemaObject { schema_id: b, .. },
        ) => a == b,
        (Message::TypedVector(a), Message::TypedVector(b)) => a.element_type == b.element_type,
        _ => false,
    }
}

fn encoded_size(message: &Message) -> usize {
    let mut codec = RecurramCodec::default();
    codec
        .encode_message(message)
        .map(|b| b.len())
        .unwrap_or(usize::MAX)
}

fn estimated_patch_size_with_base(base_ref: BaseRef, ops: &[PatchOperation]) -> usize {
    let patch = Message::StatePatch {
        base_ref,
        operations: ops.to_vec(),
        literals: Vec::new(),
    };
    encoded_size(&patch)
}

fn typed_vector_to_value(vector: TypedVector) -> Value {
    match vector.data {
        TypedVectorData::Bool(v) => Value::Array(v.into_iter().map(Value::Bool).collect()),
        TypedVectorData::I64(v) => Value::Array(v.into_iter().map(Value::I64).collect()),
        TypedVectorData::U64(v) => Value::Array(v.into_iter().map(Value::U64).collect()),
        TypedVectorData::F64(v) => Value::Array(v.into_iter().map(Value::F64).collect()),
        TypedVectorData::String(v) => Value::Array(v.into_iter().map(Value::String).collect()),
        TypedVectorData::Binary(v) => Value::Array(v.into_iter().map(Value::Binary).collect()),
        TypedVectorData::Value(v) => Value::Array(v),
    }
}

fn entries_to_map(entries: Vec<MapEntry>, state: &SessionState) -> Result<Vec<(String, Value)>> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let key = match entry.key {
            KeyRef::Literal(v) => v,
            KeyRef::Id(id) => state
                .key_table
                .get_value(id)
                .ok_or(match state.options.unknown_reference_policy {
                    UnknownReferencePolicy::FailFast => RecurramError::UnknownReference("key_id", id),
                    UnknownReferencePolicy::StatelessRetry => {
                        RecurramError::StatelessRetryRequired("key_id", id)
                    }
                })?
                .to_string(),
        };
        out.push((key, entry.value));
    }
    Ok(out)
}

fn shape_values_to_map(
    keys: &[String],
    presence: Option<Vec<bool>>,
    values: Vec<Value>,
) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let mut value_iter = values.into_iter();
    for (idx, key) in keys.iter().enumerate() {
        let present = presence
            .as_ref()
            .and_then(|bits| bits.get(idx).copied())
            .unwrap_or(true);
        if !present {
            continue;
        }
        if let Some(value) = value_iter.next() {
            out.push((key.clone(), value));
        }
    }
    out
}

fn rows_to_columns(rows: &[Vec<Value>]) -> Vec<Column> {
    if rows.is_empty() {
        return Vec::new();
    }
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut cols = Vec::with_capacity(width);
    for col_idx in 0..width {
        let mut values = Vec::new();
        let mut present_bits = Vec::new();
        for row in rows {
            let value = row.get(col_idx).cloned().unwrap_or(Value::Null);
            present_bits.push(!matches!(value, Value::Null));
            values.push(value);
        }
        let null_count = values.iter().filter(|v| matches!(v, Value::Null)).count();
        let optional_count = values.len();
        let (null_strategy, presence) = if null_count == 0 {
            (NullStrategy::AllPresentElided, None)
        } else if null_count <= optional_count / 4 {
            (
                NullStrategy::InvertedPresenceBitmap,
                Some(present_bits.iter().map(|present| !present).collect()),
            )
        } else {
            (NullStrategy::PresenceBitmap, Some(present_bits))
        };
        let values = strip_nulls(values);
        let (codec, typed_values) = infer_column_codec_and_values(&values);
        cols.push(Column {
            field_id: col_idx as u64,
            null_strategy,
            presence,
            codec,
            dictionary_id: None,
            values: typed_values,
        });
    }
    cols
}

fn strip_nulls(values: Vec<Value>) -> Vec<Value> {
    values
        .into_iter()
        .filter(|v| !matches!(v, Value::Null))
        .collect()
}

fn infer_column_codec_and_values(values: &[Value]) -> (VectorCodec, TypedVectorData) {
    if values.is_empty() {
        return (VectorCodec::Plain, TypedVectorData::Value(Vec::new()));
    }
    if values.iter().all(|v| matches!(v, Value::Bool(_))) {
        let vals = values
            .iter()
            .map(|v| match v {
                Value::Bool(b) => *b,
                _ => false,
            })
            .collect::<Vec<_>>();
        return (VectorCodec::DirectBitpack, TypedVectorData::Bool(vals));
    }
    if values.iter().all(|v| matches!(v, Value::I64(_))) {
        let vals = values
            .iter()
            .map(|v| match v {
                Value::I64(i) => *i,
                _ => 0,
            })
            .collect::<Vec<_>>();
        let codec = select_integer_codec(&vals);
        return (codec, TypedVectorData::I64(vals));
    }
    if values.iter().all(|v| matches!(v, Value::U64(_))) {
        let vals = values
            .iter()
            .map(|v| match v {
                Value::U64(i) => *i,
                _ => 0,
            })
            .collect::<Vec<_>>();
        let codec = select_u64_codec(&vals);
        return (codec, TypedVectorData::U64(vals));
    }
    if values.iter().all(|v| matches!(v, Value::F64(_))) {
        let vals = values
            .iter()
            .map(|v| match v {
                Value::F64(f) => *f,
                _ => 0.0,
            })
            .collect::<Vec<_>>();
        let codec = select_float_codec(&vals);
        return (codec, TypedVectorData::F64(vals));
    }
    if values.iter().all(|v| matches!(v, Value::String(_))) {
        let vals = values
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>();
        let codec = select_string_codec(&vals);
        return (codec, TypedVectorData::String(vals));
    }
    (VectorCodec::Plain, TypedVectorData::Value(values.to_vec()))
}

fn select_integer_codec(values: &[i64]) -> VectorCodec {
    if values.len() < 4 {
        return VectorCodec::Plain;
    }
    let delta_vals = deltas(values);
    let dd = deltas(&delta_vals);
    let non_zero_dd = dd.iter().skip(1).filter(|d| **d != 0).count();
    let non_zero_ratio = if dd.len() <= 1 {
        0.0
    } else {
        non_zero_dd as f64 / (dd.len() - 1) as f64
    };
    let delta_range_bits = bit_width_signed(
        delta_vals.iter().copied().min().unwrap_or(0),
        delta_vals.iter().copied().max().unwrap_or(0),
    );
    if values.len() >= 8 && (non_zero_ratio <= 0.25 || delta_range_bits <= 2) {
        return VectorCodec::DeltaDeltaBitpack;
    }

    let (repeated_ratio, avg_run) = run_stats(values);
    if repeated_ratio >= 0.5 && avg_run >= 3.0 {
        return VectorCodec::Rle;
    }

    let plain_bits = 64i32;
    let range_bits = bit_width_signed(
        values.iter().copied().min().unwrap_or(0),
        values.iter().copied().max().unwrap_or(0),
    ) as i32;
    if range_bits <= plain_bits - 4 {
        return VectorCodec::ForBitpack;
    }

    let monotonic = values.windows(2).all(|w| w[1] >= w[0]);
    if values.len() >= 8 && monotonic && (delta_range_bits as i32) <= (range_bits - 3) {
        return VectorCodec::DeltaForBitpack;
    }

    let max_abs_delta_bits = delta_vals
        .iter()
        .map(|d| bit_width_u64(d.unsigned_abs()))
        .max()
        .unwrap_or(1) as i32;
    if max_abs_delta_bits <= plain_bits - 3 {
        return VectorCodec::DeltaBitpack;
    }

    let max_bit_width = values
        .iter()
        .map(|v| bit_width_u64(v.unsigned_abs()))
        .max()
        .unwrap_or(1);
    if values.len() >= 8 && max_bit_width <= 16 && !monotonic {
        return VectorCodec::Simple8b;
    }

    let max_width = max_bit_width;

    if max_width < 64 {
        VectorCodec::DirectBitpack
    } else {
        VectorCodec::Plain
    }
}

fn select_u64_codec(values: &[u64]) -> VectorCodec {
    if values.iter().all(|v| i64::try_from(*v).is_ok()) {
        let signed: Vec<i64> = values.iter().map(|v| *v as i64).collect();
        return match select_integer_codec(&signed) {
            VectorCodec::Rle => VectorCodec::Rle,
            VectorCodec::ForBitpack => VectorCodec::ForBitpack,
            VectorCodec::Simple8b => VectorCodec::Simple8b,
            VectorCodec::DirectBitpack => VectorCodec::DirectBitpack,
            VectorCodec::Plain => VectorCodec::Plain,
            _ => VectorCodec::DirectBitpack,
        };
    }
    if values.len() < 4 {
        return VectorCodec::DirectBitpack;
    }

    let (repeated_ratio, avg_run) = run_stats_u64(values);
    if repeated_ratio >= 0.5 && avg_run >= 3.0 {
        return VectorCodec::Rle;
    }

    let min = values.iter().copied().min().unwrap_or(0);
    let max = values.iter().copied().max().unwrap_or(0);
    let range = max.saturating_sub(min);
    let range_bits = bit_width_u64(range) as i32;
    if range_bits <= 60 {
        return VectorCodec::ForBitpack;
    }

    let width = values.iter().map(|v| bit_width_u64(*v)).max().unwrap_or(1);
    if values.len() >= 8 && width <= 16 {
        return VectorCodec::Simple8b;
    }
    if width < 64 {
        VectorCodec::DirectBitpack
    } else {
        VectorCodec::Plain
    }
}

fn select_float_codec(values: &[f64]) -> VectorCodec {
    if values.len() < 4 {
        return VectorCodec::Plain;
    }
    let mut xor_words = Vec::with_capacity(values.len().saturating_sub(1));
    for pair in values.windows(2) {
        xor_words.push(pair[0].to_bits() ^ pair[1].to_bits());
    }
    let zero_or_one = xor_words
        .iter()
        .filter(|x| **x == 0 || x.count_ones() <= 1)
        .count();
    let avg_non_zero_width = {
        let mut widths = xor_words
            .iter()
            .filter(|x| **x != 0)
            .map(|x| bit_width_u64(*x) as f64);
        let mut sum = 0.0;
        let mut n = 0.0;
        for width in widths.by_ref() {
            sum += width;
            n += 1.0;
        }
        if n == 0.0 { 0.0 } else { sum / n }
    };
    let ratio = zero_or_one as f64 / xor_words.len().max(1) as f64;
    if ratio >= 0.5 && avg_non_zero_width <= 16.0 {
        VectorCodec::XorFloat
    } else {
        VectorCodec::Plain
    }
}

fn select_string_codec(values: &[String]) -> VectorCodec {
    if values.len() < 4 {
        return VectorCodec::Plain;
    }
    let mut prefix_hits = 0usize;
    for pair in values.windows(2) {
        if common_prefix_len(pair[0].as_bytes(), pair[1].as_bytes()) >= 3 {
            prefix_hits += 1;
        }
    }
    let prefix_ratio = prefix_hits as f64 / values.len().saturating_sub(1).max(1) as f64;
    if values.len() >= 4 && prefix_ratio >= 0.5 {
        return VectorCodec::PrefixDelta;
    }
    let mut unique = std::collections::BTreeSet::new();
    for value in values {
        unique.insert(value.as_str());
    }
    let unique_ratio = unique.len() as f64 / values.len() as f64;
    if values.len() >= 16 && unique_ratio <= 0.25 {
        return VectorCodec::Dictionary;
    }
    if unique.len() < values.len() {
        return VectorCodec::StringRef;
    }
    VectorCodec::Plain
}

fn deltas(values: &[i64]) -> Vec<i64> {
    let mut out = Vec::with_capacity(values.len());
    for (idx, value) in values.iter().enumerate() {
        if idx == 0 {
            out.push(*value);
        } else {
            out.push(*value - values[idx - 1]);
        }
    }
    out
}

fn run_stats(values: &[i64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let mut run_len = 1usize;
    let mut runs = Vec::new();
    for pair in values.windows(2) {
        if pair[0] == pair[1] {
            run_len += 1;
        } else {
            runs.push(run_len);
            run_len = 1;
        }
    }
    runs.push(run_len);
    let repeated_items: usize = runs.iter().filter(|r| **r > 1).copied().sum();
    let repeated_ratio = repeated_items as f64 / values.len() as f64;
    let avg_run = runs.iter().copied().sum::<usize>() as f64 / runs.len() as f64;
    (repeated_ratio, avg_run)
}

fn run_stats_u64(values: &[u64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let mut run_len = 1usize;
    let mut runs = Vec::new();
    for pair in values.windows(2) {
        if pair[0] == pair[1] {
            run_len += 1;
        } else {
            runs.push(run_len);
            run_len = 1;
        }
    }
    runs.push(run_len);
    let repeated_items: usize = runs.iter().filter(|r| **r > 1).copied().sum();
    let repeated_ratio = repeated_items as f64 / values.len() as f64;
    let avg_run = runs.iter().copied().sum::<usize>() as f64 / runs.len() as f64;
    (repeated_ratio, avg_run)
}

fn bit_width_signed(min: i64, max: i64) -> u8 {
    let range = max.saturating_sub(min).unsigned_abs();
    bit_width_u64(range)
}

fn bit_width_u64(v: u64) -> u8 {
    if v == 0 {
        1
    } else {
        (u64::BITS - v.leading_zeros()) as u8
    }
}

fn range_bit_width_u64(min: u64, max: u64) -> u8 {
    let span = max.saturating_sub(min);
    if span == 0 {
        0
    } else {
        (u64::BITS - span.leading_zeros()) as u8
    }
}

fn range_bit_width_i64(min: i64, max: i64) -> u8 {
    let span = i128::from(max) - i128::from(min);
    if span == 0 {
        0
    } else {
        (u128::BITS - (span as u128).leading_zeros()) as u8
    }
}

fn write_fixed_bits_u64(value: u64, bits: u8, out: &mut Vec<u8>) -> Result<()> {
    if bits > 64 {
        return Err(RecurramError::InvalidData("fixed bit width"));
    }
    if bits == 0 {
        if value != 0 {
            return Err(RecurramError::InvalidData("fixed bit width value overflow"));
        }
        return Ok(());
    }
    if bits < 64 && (value >> bits) != 0 {
        return Err(RecurramError::InvalidData("fixed bit width value overflow"));
    }
    let byte_len = usize::from(bits).div_ceil(8);
    for idx in 0..byte_len {
        out.push(((value >> (idx * 8)) & 0xFF) as u8);
    }
    Ok(())
}

fn read_fixed_bits_u64(reader: &mut Reader<'_>, bits: u8) -> Result<u64> {
    if bits > 64 {
        return Err(RecurramError::InvalidData("fixed bit width"));
    }
    if bits == 0 {
        return Ok(0);
    }
    let byte_len = usize::from(bits).div_ceil(8);
    let mut value = 0u64;
    for idx in 0..byte_len {
        value |= u64::from(reader.read_u8()?) << (idx * 8);
    }
    if bits < 64 {
        let mask = (1u64 << bits) - 1;
        if (value & !mask) != 0 {
            return Err(RecurramError::InvalidData("fixed bit width trailing bits"));
        }
    }
    Ok(value)
}

fn write_smallest_u64(value: u64, out: &mut Vec<u8>) {
    if u8::try_from(value).is_ok() {
        out.push(1);
        out.push(value as u8);
    } else if u16::try_from(value).is_ok() {
        out.push(2);
        out.extend_from_slice(&(value as u16).to_le_bytes());
    } else if u32::try_from(value).is_ok() {
        out.push(4);
        out.extend_from_slice(&(value as u32).to_le_bytes());
    } else {
        out.push(8);
        out.extend_from_slice(&value.to_le_bytes());
    }
}

fn read_smallest_u64(reader: &mut Reader<'_>) -> Result<u64> {
    match reader.read_u8()? {
        1 => Ok(reader.read_u8()? as u64),
        2 => {
            let mut b = [0u8; 2];
            b.copy_from_slice(reader.read_exact(2)?);
            Ok(u16::from_le_bytes(b) as u64)
        }
        4 => {
            let mut b = [0u8; 4];
            b.copy_from_slice(reader.read_exact(4)?);
            Ok(u32::from_le_bytes(b) as u64)
        }
        8 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(reader.read_exact(8)?);
            Ok(u64::from_le_bytes(b))
        }
        _ => Err(RecurramError::InvalidData("smallest-width integer size")),
    }
}

fn rle_encode_bytes(input: &[u8]) -> Vec<u8> {
    if input.is_empty() {
        let mut out = Vec::new();
        encode_varuint(0, &mut out);
        return out;
    }
    let mut runs: Vec<(u8, u64)> = Vec::new();
    for byte in input {
        if let Some((last, len)) = runs.last_mut()
            && *last == *byte
        {
            *len += 1;
        } else {
            runs.push((*byte, 1));
        }
    }
    let mut out = Vec::new();
    encode_varuint(runs.len() as u64, &mut out);
    for (byte, len) in runs {
        encode_varuint(len, &mut out);
        out.push(byte);
    }
    out
}

fn rle_decode_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut reader = Reader::new(input);
    let run_count = reader.read_varuint()? as usize;
    let mut out = Vec::new();
    for _ in 0..run_count {
        let len = reader.read_varuint()? as usize;
        let byte = reader.read_u8()?;
        out.extend(std::iter::repeat_n(byte, len));
    }
    if !reader.is_eof() {
        return Err(RecurramError::InvalidData("control stream rle trailing bytes"));
    }
    Ok(out)
}

fn control_bitpack_encode_bytes(input: &[u8]) -> Vec<u8> {
    if input.is_empty() {
        return vec![0];
    }

    let max = input.iter().copied().max().unwrap_or(0);
    let mut best = {
        let mut raw = Vec::with_capacity(input.len() + 1);
        raw.push(0);
        raw.extend_from_slice(input);
        raw
    };

    let width = if max <= 1 {
        Some(1u8)
    } else if max <= 3 {
        Some(2u8)
    } else if max <= 15 {
        Some(4u8)
    } else {
        None
    };

    if let Some(width) = width {
        let mut candidate = Vec::new();
        candidate.push(width);
        encode_varuint(input.len() as u64, &mut candidate);
        pack_fixed_width_u8(input, width, &mut candidate);
        if candidate.len() < best.len() {
            best = candidate;
        }
    }

    best
}

fn control_bitpack_decode_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut reader = Reader::new(input);
    let mode = reader.read_u8()?;
    match mode {
        0 => {
            let remaining = input.len().saturating_sub(reader.position());
            Ok(reader.read_exact(remaining)?.to_vec())
        }
        1 | 2 | 4 => {
            let len = reader.read_varuint()? as usize;
            let remaining = input.len().saturating_sub(reader.position());
            let packed = reader.read_exact(remaining)?;
            unpack_fixed_width_u8(packed, len, mode)
        }
        _ => Err(RecurramError::InvalidData("control stream bitpack mode")),
    }
}

fn control_huffman_encode_bytes(input: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(input.len() + 1);
    raw.push(0);
    raw.extend_from_slice(input);

    if input.is_empty() {
        return raw;
    }

    let mut freqs = [0u32; 256];
    for byte in input {
        freqs[*byte as usize] = freqs[*byte as usize].saturating_add(1);
    }
    let Some((nodes, root)) = build_huffman_tree(&freqs) else {
        return raw;
    };

    let codebook = build_huffman_codebook(&nodes, root);
    let mut bitstream = Vec::new();
    let mut acc = 0u8;
    let mut bit_count = 0u8;
    for byte in input {
        for bit in &codebook[*byte as usize] {
            if *bit == 1 {
                acc |= 1 << bit_count;
            }
            bit_count += 1;
            if bit_count == 8 {
                bitstream.push(acc);
                acc = 0;
                bit_count = 0;
            }
        }
    }
    if bit_count > 0 {
        bitstream.push(acc);
    }

    let mut huff = Vec::new();
    huff.push(1);
    let used = freqs.iter().filter(|f| **f > 0).count() as u64;
    encode_varuint(used, &mut huff);
    for (symbol, freq) in freqs.iter().enumerate() {
        if *freq == 0 {
            continue;
        }
        huff.push(symbol as u8);
        encode_varuint(*freq as u64, &mut huff);
    }
    huff.extend_from_slice(&bitstream);

    if huff.len() < raw.len() { huff } else { raw }
}

fn control_huffman_decode_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut reader = Reader::new(input);
    let mode = reader.read_u8()?;
    match mode {
        0 => {
            let remaining = input.len().saturating_sub(reader.position());
            Ok(reader.read_exact(remaining)?.to_vec())
        }
        1 => {
            let used = reader.read_varuint()? as usize;
            let mut freqs = [0u32; 256];
            for _ in 0..used {
                let symbol = reader.read_u8()? as usize;
                let freq = reader.read_varuint()?;
                if freq > u32::MAX as u64 {
                    return Err(RecurramError::InvalidData("control stream huffman freq"));
                }
                freqs[symbol] = freq as u32;
            }
            let total = freqs.iter().map(|f| *f as usize).sum::<usize>();
            if total == 0 {
                return Ok(Vec::new());
            }
            let (nodes, root) = build_huffman_tree(&freqs)
                .ok_or(RecurramError::InvalidData("control stream huffman tree"))?;
            if let HuffNode::Leaf(symbol) = nodes[root] {
                return Ok(std::iter::repeat_n(symbol, total).collect());
            }

            let remaining = input.len().saturating_sub(reader.position());
            let bitstream = reader.read_exact(remaining)?;
            let mut out = Vec::with_capacity(total);
            let mut byte_idx = 0usize;
            let mut bit_idx = 0u8;
            for _ in 0..total {
                let mut node_idx = root;
                loop {
                    match nodes[node_idx] {
                        HuffNode::Leaf(symbol) => {
                            out.push(symbol);
                            break;
                        }
                        HuffNode::Internal { left, right } => {
                            let byte = *bitstream.get(byte_idx).ok_or(RecurramError::InvalidData(
                                "control stream huffman underflow",
                            ))?;
                            let bit = (byte >> bit_idx) & 1;
                            bit_idx += 1;
                            if bit_idx == 8 {
                                bit_idx = 0;
                                byte_idx += 1;
                            }
                            node_idx = if bit == 0 { left } else { right };
                        }
                    }
                }
            }
            if bit_idx > 0 && byte_idx < bitstream.len() {
                let trailing_mask = !((1u8 << bit_idx) - 1);
                if (bitstream[byte_idx] & trailing_mask) != 0 {
                    return Err(RecurramError::InvalidData(
                        "control stream huffman trailing bits",
                    ));
                }
                byte_idx += 1;
            }
            if bitstream[byte_idx..].iter().any(|b| *b != 0) {
                return Err(RecurramError::InvalidData(
                    "control stream huffman trailing bits",
                ));
            }
            Ok(out)
        }
        _ => Err(RecurramError::InvalidData("control stream huffman mode")),
    }
}

fn control_fse_encode_bytes(input: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(input.len() + 1);
    raw.push(0);
    raw.extend_from_slice(input);
    if input.is_empty() {
        return raw;
    }
    control_fse_frame_encode(input).unwrap_or(raw)
}

fn control_fse_decode_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut reader = Reader::new(input);
    let mode = reader.read_u8()?;
    let remaining = input.len().saturating_sub(reader.position());
    let body = reader.read_exact(remaining)?;
    match mode {
        0 => Ok(body.to_vec()),
        1 => control_bitpack_decode_bytes(body),
        2 => control_huffman_decode_bytes(body),
        3 => control_fse_frame_decode(body),
        _ => Err(RecurramError::InvalidData("control stream fse mode")),
    }
}

const FSE_TABLE_LOG: u8 = 8;
const FSE_STATE_LOWER_BOUND: u32 = 1 << 23;

fn control_fse_frame_encode(input: &[u8]) -> Option<Vec<u8>> {
    if input.is_empty() {
        return None;
    }

    let mut counts = [0u32; 256];
    for byte in input {
        counts[*byte as usize] = counts[*byte as usize].saturating_add(1);
    }
    let table_size = 1u32 << FSE_TABLE_LOG;
    let freqs = normalize_fse_frequencies(&counts, table_size)?;

    let mut cumul = [0u32; 256];
    let mut sum = 0u32;
    for (idx, freq) in freqs.iter().copied().enumerate() {
        cumul[idx] = sum;
        sum = sum.saturating_add(u32::from(freq));
    }
    if sum != table_size {
        return None;
    }

    let mut renorm = Vec::new();
    let mut state = FSE_STATE_LOWER_BOUND;
    for symbol in input.iter().rev() {
        let freq = u32::from(freqs[*symbol as usize]);
        if freq == 0 {
            return None;
        }
        let x_max = (((FSE_STATE_LOWER_BOUND >> FSE_TABLE_LOG) << 8) as u64)
            .saturating_mul(u64::from(freq));
        while u64::from(state) >= x_max {
            renorm.push((state & 0xFF) as u8);
            state >>= 8;
        }

        let q = state / freq;
        let r = state % freq;
        state = (q << FSE_TABLE_LOG) + r + cumul[*symbol as usize];
    }

    let mut out = Vec::new();
    out.push(3);
    out.push(FSE_TABLE_LOG);
    encode_varuint(input.len() as u64, &mut out);

    let used = freqs.iter().filter(|freq| **freq > 0).count() as u64;
    encode_varuint(used, &mut out);
    for (symbol, freq) in freqs.iter().copied().enumerate() {
        if freq == 0 {
            continue;
        }
        out.push(symbol as u8);
        encode_varuint(u64::from(freq), &mut out);
    }
    encode_varuint(u64::from(state), &mut out);
    out.extend_from_slice(&renorm);
    Some(out)
}

fn control_fse_frame_decode(input: &[u8]) -> Result<Vec<u8>> {
    let mut reader = Reader::new(input);
    let table_log = reader.read_u8()?;
    if table_log == 0 || table_log > 12 {
        return Err(RecurramError::InvalidData("control stream fse table log"));
    }
    let table_size = 1u32 << table_log;
    let len = reader.read_varuint()? as usize;
    let used = reader.read_varuint()? as usize;
    if used > 256 || used > table_size as usize {
        return Err(RecurramError::InvalidData("control stream fse used symbols"));
    }

    let mut freqs = [0u16; 256];
    let mut seen = [false; 256];
    let mut sum = 0u32;
    for _ in 0..used {
        let symbol = reader.read_u8()? as usize;
        if seen[symbol] {
            return Err(RecurramError::InvalidData(
                "control stream fse duplicate symbol",
            ));
        }
        seen[symbol] = true;
        let freq = reader.read_varuint()?;
        if freq == 0 || freq > table_size as u64 {
            return Err(RecurramError::InvalidData("control stream fse freq"));
        }
        let freq_u16 =
            u16::try_from(freq).map_err(|_| RecurramError::InvalidData("control stream fse freq"))?;
        freqs[symbol] = freq_u16;
        sum = sum
            .checked_add(u32::from(freq_u16))
            .ok_or(RecurramError::InvalidData("control stream fse table sum"))?;
    }
    if sum != table_size {
        return Err(RecurramError::InvalidData("control stream fse table sum"));
    }

    let mut cumul = [0u32; 256];
    let mut running = 0u32;
    for (idx, freq) in freqs.iter().copied().enumerate() {
        cumul[idx] = running;
        running = running.saturating_add(u32::from(freq));
    }

    let mut decode_table = vec![0u8; table_size as usize];
    for (symbol, freq) in freqs.iter().copied().enumerate() {
        let freq_u32 = u32::from(freq);
        if freq_u32 == 0 {
            continue;
        }
        let start = cumul[symbol];
        for slot in start..start + freq_u32 {
            decode_table[slot as usize] = symbol as u8;
        }
    }

    let state = reader.read_varuint()?;
    if state > u64::from(u32::MAX) {
        return Err(RecurramError::InvalidData("control stream fse state"));
    }
    let mut state = state as u32;

    let remaining = input.len().saturating_sub(reader.position());
    let renorm = reader.read_exact(remaining)?;
    let mut renorm_idx = renorm.len();
    let mask = table_size - 1;

    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        let slot = (state & mask) as usize;
        let symbol = *decode_table
            .get(slot)
            .ok_or(RecurramError::InvalidData("control stream fse decode table"))?;
        out.push(symbol);

        let freq = u32::from(freqs[symbol as usize]);
        if freq == 0 {
            return Err(RecurramError::InvalidData("control stream fse symbol freq"));
        }
        let start = cumul[symbol as usize];
        let low = state & mask;
        let delta = low
            .checked_sub(start)
            .ok_or(RecurramError::InvalidData("control stream fse state"))?;
        let base = freq
            .checked_mul(state >> table_log)
            .ok_or(RecurramError::InvalidData("control stream fse state"))?;
        state = base
            .checked_add(delta)
            .ok_or(RecurramError::InvalidData("control stream fse state"))?;

        while state < FSE_STATE_LOWER_BOUND {
            if renorm_idx == 0 {
                return Err(RecurramError::InvalidData("control stream fse underflow"));
            }
            renorm_idx -= 1;
            state = (state << 8) | u32::from(renorm[renorm_idx]);
        }
    }

    if renorm[..renorm_idx].iter().any(|byte| *byte != 0) {
        return Err(RecurramError::InvalidData("control stream fse trailing bytes"));
    }
    Ok(out)
}

fn normalize_fse_frequencies(counts: &[u32; 256], table_size: u32) -> Option<[u16; 256]> {
    let total: u64 = counts.iter().map(|count| u64::from(*count)).sum();
    if total == 0 {
        return None;
    }

    let mut freqs = [0u32; 256];
    let mut used = 0usize;
    let mut sum = 0u32;
    for (symbol, count) in counts.iter().copied().enumerate() {
        if count == 0 {
            continue;
        }
        used += 1;
        let scaled = (u64::from(count) * u64::from(table_size)) / total;
        let freq = scaled.max(1) as u32;
        freqs[symbol] = freq;
        sum = sum.saturating_add(freq);
    }
    if used == 0 || used > table_size as usize {
        return None;
    }

    while sum < table_size {
        let mut best_symbol = None;
        let mut best_remainder = i64::MIN;
        for (symbol, count) in counts.iter().copied().enumerate() {
            if count == 0 {
                continue;
            }
            let exact_num = (u64::from(count) * u64::from(table_size)) as i64;
            let remainder = exact_num - (i64::from(freqs[symbol]) * total as i64);
            if remainder > best_remainder {
                best_remainder = remainder;
                best_symbol = Some(symbol);
            }
        }
        let symbol = best_symbol?;
        freqs[symbol] = freqs[symbol].saturating_add(1);
        sum = sum.saturating_add(1);
    }

    while sum > table_size {
        let mut best_symbol = None;
        let mut best_remainder = i64::MAX;
        for (symbol, count) in counts.iter().copied().enumerate() {
            if count == 0 || freqs[symbol] <= 1 {
                continue;
            }
            let exact_num = (u64::from(count) * u64::from(table_size)) as i64;
            let remainder = exact_num - (i64::from(freqs[symbol]) * total as i64);
            if remainder < best_remainder {
                best_remainder = remainder;
                best_symbol = Some(symbol);
            }
        }
        let symbol = best_symbol?;
        freqs[symbol] -= 1;
        sum -= 1;
    }

    let mut out = [0u16; 256];
    for (idx, freq) in freqs.into_iter().enumerate() {
        out[idx] = u16::try_from(freq).ok()?;
    }
    Some(out)
}

#[derive(Clone, Copy)]
enum HuffNode {
    Leaf(u8),
    Internal { left: usize, right: usize },
}

fn build_huffman_tree(freqs: &[u32; 256]) -> Option<(Vec<HuffNode>, usize)> {
    let mut nodes = Vec::<HuffNode>::new();
    let mut heap = std::collections::BinaryHeap::<std::cmp::Reverse<(u32, u16, usize)>>::new();

    for (symbol, freq) in freqs.iter().enumerate() {
        if *freq == 0 {
            continue;
        }
        let idx = nodes.len();
        nodes.push(HuffNode::Leaf(symbol as u8));
        heap.push(std::cmp::Reverse((*freq, symbol as u16, idx)));
    }
    if heap.is_empty() {
        return None;
    }
    while heap.len() > 1 {
        let std::cmp::Reverse((fa, sa, ia)) = heap.pop()?;
        let std::cmp::Reverse((fb, sb, ib)) = heap.pop()?;
        let idx = nodes.len();
        nodes.push(HuffNode::Internal {
            left: ia,
            right: ib,
        });
        heap.push(std::cmp::Reverse((fa.saturating_add(fb), sa.min(sb), idx)));
    }
    let std::cmp::Reverse((_, _, root)) = heap.pop()?;
    Some((nodes, root))
}

fn build_huffman_codebook(nodes: &[HuffNode], root: usize) -> Vec<Vec<u8>> {
    fn visit(nodes: &[HuffNode], idx: usize, path: &mut Vec<u8>, out: &mut [Vec<u8>]) {
        match nodes[idx] {
            HuffNode::Leaf(symbol) => {
                out[symbol as usize] = path.clone();
            }
            HuffNode::Internal { left, right } => {
                path.push(0);
                visit(nodes, left, path, out);
                path.pop();

                path.push(1);
                visit(nodes, right, path, out);
                path.pop();
            }
        }
    }

    let mut out = vec![Vec::new(); 256];
    let mut path = Vec::new();
    visit(nodes, root, &mut path, &mut out);
    out
}

fn pack_fixed_width_u8(values: &[u8], width: u8, out: &mut Vec<u8>) {
    let mask = (1u16 << width) - 1;
    let mut acc = 0u32;
    let mut acc_bits = 0u8;
    for value in values {
        acc |= (u32::from(*value) & u32::from(mask)) << acc_bits;
        acc_bits += width;
        while acc_bits >= 8 {
            out.push((acc & 0xFF) as u8);
            acc >>= 8;
            acc_bits -= 8;
        }
    }
    if acc_bits > 0 {
        out.push((acc & 0xFF) as u8);
    }
}

fn unpack_fixed_width_u8(bytes: &[u8], len: usize, width: u8) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    let mut acc = 0u32;
    let mut acc_bits = 0u8;
    let mut idx = 0usize;
    let mask = (1u32 << width) - 1;
    while out.len() < len {
        while acc_bits < width {
            let b = *bytes
                .get(idx)
                .ok_or(RecurramError::InvalidData("control stream bitpack underflow"))?;
            idx += 1;
            acc |= u32::from(b) << acc_bits;
            acc_bits += 8;
        }
        out.push((acc & mask) as u8);
        acc >>= width;
        acc_bits -= width;
    }
    if idx < bytes.len() {
        let trailing_non_zero = bytes[idx..].iter().any(|b| *b != 0);
        if trailing_non_zero {
            return Err(RecurramError::InvalidData(
                "control stream bitpack trailing bytes",
            ));
        }
    }
    Ok(out)
}

fn pack_fixed_width_u64(values: &[u64], width: u8, out: &mut Vec<u8>) -> Result<()> {
    if width > 64 {
        return Err(RecurramError::InvalidData("fixed-width u64 bit width"));
    }
    if width == 0 {
        if values.iter().any(|value| *value != 0) {
            return Err(RecurramError::InvalidData("fixed-width u64 value overflow"));
        }
        return Ok(());
    }

    let mut acc = 0u128;
    let mut acc_bits = 0u32;
    for value in values {
        if width < 64 && (*value >> width) != 0 {
            return Err(RecurramError::InvalidData("fixed-width u64 value overflow"));
        }
        acc |= u128::from(*value) << acc_bits;
        acc_bits += u32::from(width);
        while acc_bits >= 8 {
            out.push((acc & 0xFF) as u8);
            acc >>= 8;
            acc_bits -= 8;
        }
    }
    if acc_bits > 0 {
        out.push((acc & 0xFF) as u8);
    }
    Ok(())
}

fn unpack_fixed_width_u64(bytes: &[u8], len: usize, width: u8) -> Result<Vec<u64>> {
    if width > 64 {
        return Err(RecurramError::InvalidData("fixed-width u64 bit width"));
    }
    if width == 0 {
        if bytes.iter().any(|byte| *byte != 0) {
            return Err(RecurramError::InvalidData("fixed-width u64 trailing bytes"));
        }
        return Ok(vec![0; len]);
    }

    let mut out = Vec::with_capacity(len);
    let mut acc = 0u128;
    let mut acc_bits = 0u32;
    let mut idx = 0usize;
    let mask = if width == 64 {
        u128::from(u64::MAX)
    } else {
        (1u128 << width) - 1
    };
    for _ in 0..len {
        while acc_bits < u32::from(width) {
            let byte = *bytes
                .get(idx)
                .ok_or(RecurramError::InvalidData("fixed-width u64 underflow"))?;
            idx += 1;
            acc |= u128::from(byte) << acc_bits;
            acc_bits += 8;
        }
        out.push((acc & mask) as u64);
        acc >>= width;
        acc_bits -= u32::from(width);
    }
    if acc != 0 || bytes[idx..].iter().any(|byte| *byte != 0) {
        return Err(RecurramError::InvalidData("fixed-width u64 trailing bytes"));
    }
    Ok(out)
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let mut idx = 0usize;
    let max = a.len().min(b.len());
    while idx < max && a[idx] == b[idx] {
        idx += 1;
    }
    idx
}

fn template_descriptor_from_columns(template_id: u64, columns: &[Column]) -> TemplateDescriptor {
    TemplateDescriptor {
        template_id,
        field_ids: columns.iter().map(|c| c.field_id).collect(),
        null_strategies: columns.iter().map(|c| c.null_strategy).collect(),
        codecs: columns.iter().map(|c| c.codec).collect(),
    }
}

fn find_template_id(
    templates: &std::collections::HashMap<u64, TemplateDescriptor>,
    probe: &TemplateDescriptor,
) -> Option<u64> {
    templates
        .iter()
        .filter_map(|(id, desc)| {
            if desc.field_ids == probe.field_ids
                && desc.null_strategies == probe.null_strategies
                && desc.codecs == probe.codecs
            {
                Some(*id)
            } else {
                None
            }
        })
        .min()
}

fn diff_template_columns(previous: &[Column], current: &[Column]) -> (Vec<bool>, Vec<Column>) {
    let len = previous.len().max(current.len());
    let mut mask = Vec::with_capacity(len);
    let mut changed = Vec::new();
    for idx in 0..len {
        let p = previous.get(idx);
        let c = current.get(idx);
        if p == c {
            mask.push(false);
        } else {
            mask.push(true);
            if let Some(col) = c {
                changed.push(col.clone());
            }
        }
    }
    (mask, changed)
}

fn merge_template_columns(
    previous: &[Column],
    changed_mask: &[bool],
    changed_columns: Vec<Column>,
) -> Result<Vec<Column>> {
    let mut changed_iter = changed_columns.into_iter();
    let mut out = Vec::with_capacity(changed_mask.len());
    for (idx, changed) in changed_mask.iter().copied().enumerate() {
        if changed {
            out.push(
                changed_iter
                    .next()
                    .ok_or(RecurramError::InvalidData("template changed column count"))?,
            );
        } else {
            let base = previous
                .get(idx)
                .ok_or(RecurramError::InvalidData("template base column missing"))?;
            out.push(base.clone());
        }
    }
    if changed_iter.next().is_some() {
        return Err(RecurramError::InvalidData("template changed column overflow"));
    }
    Ok(out)
}

fn decode_trained_dictionary_payload(payload: &[u8]) -> Result<Vec<String>> {
    let mut reader = Reader::new(payload);
    let len = reader.read_varuint()? as usize;
    let mut values = Vec::with_capacity(len);
    for _ in 0..len {
        values.push(reader.read_string()?);
    }
    if !reader.is_eof() {
        return Err(RecurramError::InvalidData(
            "trained dictionary payload trailing bytes",
        ));
    }
    Ok(values)
}

fn encode_trained_dictionary_block(
    values: &[String],
    dictionary: &[String],
) -> Result<Option<Vec<u8>>> {
    if values.is_empty() {
        let mut out = Vec::new();
        out.push(0);
        encode_varuint(0, &mut out);
        return Ok(Some(out));
    }

    let mut by_value = std::collections::BTreeMap::<String, u64>::new();
    for (idx, value) in dictionary.iter().enumerate() {
        by_value.insert(value.clone(), idx as u64);
    }

    let mut ids = Vec::with_capacity(values.len());
    for value in values {
        let Some(id) = by_value.get(value).copied() else {
            return Ok(None);
        };
        ids.push(id);
    }

    let mut raw = Vec::new();
    raw.push(0);
    encode_varuint(ids.len() as u64, &mut raw);
    for id in &ids {
        encode_varuint(*id, &mut raw);
    }

    let max_id = ids.iter().copied().max().unwrap_or(0);
    let bit_width = if max_id == 0 {
        0
    } else {
        (u64::BITS - max_id.leading_zeros()) as u8
    };
    let mut packed = Vec::new();
    pack_fixed_width_u64(&ids, bit_width, &mut packed)?;
    let mut bitpacked = Vec::new();
    bitpacked.push(1);
    encode_varuint(ids.len() as u64, &mut bitpacked);
    bitpacked.push(bit_width);
    bitpacked.extend_from_slice(&packed);

    if bitpacked.len() < raw.len() {
        Ok(Some(bitpacked))
    } else {
        Ok(Some(raw))
    }
}

fn decode_trained_dictionary_block(block: &[u8], dictionary: &[String]) -> Result<Vec<String>> {
    let mut reader = Reader::new(block);
    let mode = reader.read_u8()?;
    let len = reader.read_varuint()? as usize;
    let ids = match mode {
        0 => {
            let mut ids = Vec::with_capacity(len);
            for _ in 0..len {
                ids.push(reader.read_varuint()?);
            }
            ids
        }
        1 => {
            let bit_width = reader.read_u8()?;
            let remaining = block.len().saturating_sub(reader.position());
            let packed = reader.read_exact(remaining)?;
            unpack_fixed_width_u64(packed, len, bit_width)?
        }
        _ => return Err(RecurramError::InvalidData("trained dictionary block mode")),
    };
    if !reader.is_eof() {
        return Err(RecurramError::InvalidData(
            "trained dictionary block trailing bytes",
        ));
    }

    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let idx = usize::try_from(id)
            .map_err(|_| RecurramError::InvalidData("trained dictionary block id"))?;
        let value = dictionary
            .get(idx)
            .cloned()
            .ok_or(RecurramError::InvalidData("trained dictionary block id"))?;
        out.push(value);
    }
    Ok(out)
}

fn apply_dictionary_references(state: &mut SessionState, columns: &mut [Column]) {
    for column in columns.iter_mut() {
        let TypedVectorData::String(values) = &column.values else {
            continue;
        };
        if values.len() < 16 {
            continue;
        }
        let unique = values
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let unique_ratio = unique.len() as f64 / values.len() as f64;
        if unique_ratio > 0.5 {
            continue;
        }
        if !matches!(
            column.codec,
            VectorCodec::Dictionary | VectorCodec::StringRef
        ) {
            continue;
        }
        let dict_id = state.allocate_dictionary_id();
        let mut payload = Vec::new();
        encode_varuint(unique.len() as u64, &mut payload);
        for item in unique {
            encode_string(&item, &mut payload);
        }
        let profile = DictionaryProfile {
            version: 1,
            hash: dictionary_payload_hash(&payload),
            expires_at: 0,
            fallback: match state.options.unknown_reference_policy {
                UnknownReferencePolicy::FailFast => DictionaryFallback::FailFast,
                UnknownReferencePolicy::StatelessRetry => DictionaryFallback::StatelessRetry,
            },
        };
        state.dictionaries.insert(dict_id, payload);
        state.dictionary_profiles.insert(dict_id, profile);
        column.dictionary_id = Some(dict_id);
    }
}

fn dictionary_payload_hash(payload: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for byte in payload {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn diff_message(prev: &Message, current: &Message) -> (Vec<PatchOperation>, usize) {
    let prev_fields = message_fields(prev);
    let curr_fields = message_fields(current);
    let max_len = prev_fields.len().max(curr_fields.len());
    let mut ops = Vec::with_capacity(max_len);
    let mut changed = 0usize;
    for idx in 0..max_len {
        let p = prev_fields.get(idx);
        let c = curr_fields.get(idx);
        let op = match (p, c) {
            (Some(a), Some(b)) if a == b => continue,
            (Some(_), Some(b)) => {
                changed += 1;
                PatchOperation {
                    field_id: idx as u64,
                    opcode: PatchOpcode::ReplaceScalar,
                    value: Some(b.clone()),
                }
            }
            (Some(_), None) => {
                changed += 1;
                PatchOperation {
                    field_id: idx as u64,
                    opcode: PatchOpcode::DeleteField,
                    value: None,
                }
            }
            (None, Some(b)) => {
                changed += 1;
                PatchOperation {
                    field_id: idx as u64,
                    opcode: PatchOpcode::InsertField,
                    value: Some(b.clone()),
                }
            }
            (None, None) => continue,
        };
        ops.push(op);
    }
    (ops, changed)
}

fn apply_state_patch_map(
    base_entries: &[MapEntry],
    operations: &[PatchOperation],
    literals: &[Value],
) -> Result<Message> {
    let mut entries = base_entries.to_vec();
    let mut literal_iter = literals.iter().cloned();
    for operation in operations {
        let idx = operation.field_id as usize;
        match operation.opcode {
            PatchOpcode::Keep => {}
            PatchOpcode::ReplaceScalar
            | PatchOpcode::ReplaceVector
            | PatchOpcode::StringRef
            | PatchOpcode::PrefixDelta => {
                let value = operation
                    .value
                    .clone()
                    .or_else(|| literal_iter.next())
                    .ok_or(RecurramError::InvalidData("patch replace missing value"))?;
                if idx >= entries.len() {
                    return Err(RecurramError::InvalidData("patch field out of bounds"));
                }
                entries[idx].value = value;
            }
            PatchOpcode::InsertField => {
                let value = operation
                    .value
                    .clone()
                    .or_else(|| literal_iter.next())
                    .ok_or(RecurramError::InvalidData("patch insert missing value"))?;
                if idx > entries.len() {
                    return Err(RecurramError::InvalidData("patch insert out of bounds"));
                }
                entries.insert(idx, map_entry_from_patch_value(value)?);
            }
            PatchOpcode::DeleteField => {
                if idx >= entries.len() {
                    return Err(RecurramError::InvalidData("patch delete out of bounds"));
                }
                entries.remove(idx);
            }
            PatchOpcode::AppendVector => {
                let value = operation
                    .value
                    .clone()
                    .or_else(|| literal_iter.next())
                    .ok_or(RecurramError::InvalidData("patch append missing value"))?;
                if idx >= entries.len() {
                    return Err(RecurramError::InvalidData("patch append out of bounds"));
                }
                match (&mut entries[idx].value, value) {
                    (Value::Array(dst), Value::Array(mut src)) => dst.append(&mut src),
                    _ => return Err(RecurramError::InvalidData("patch append type mismatch")),
                }
            }
            PatchOpcode::TruncateVector => {
                let value = operation
                    .value
                    .clone()
                    .or_else(|| literal_iter.next())
                    .ok_or(RecurramError::InvalidData("patch truncate missing value"))?;
                if idx >= entries.len() {
                    return Err(RecurramError::InvalidData("patch truncate out of bounds"));
                }
                let keep = match value {
                    Value::U64(v) => v as usize,
                    Value::I64(v) if v >= 0 => v as usize,
                    _ => return Err(RecurramError::InvalidData("patch truncate count")),
                };
                match &mut entries[idx].value {
                    Value::Array(dst) => dst.truncate(keep),
                    _ => return Err(RecurramError::InvalidData("patch truncate type mismatch")),
                }
            }
        }
    }
    Ok(Message::Map(entries))
}

fn map_entry_from_patch_value(value: Value) -> Result<MapEntry> {
    let Value::Map(mut entries) = value else {
        return Err(RecurramError::InvalidData(
            "patch map insert requires single-entry map value",
        ));
    };
    if entries.len() != 1 {
        return Err(RecurramError::InvalidData(
            "patch map insert requires single-entry map value",
        ));
    }
    let (key, value) = entries.remove(0);
    Ok(MapEntry {
        key: KeyRef::Literal(key),
        value,
    })
}

fn message_fields(message: &Message) -> Vec<Value> {
    match message {
        Message::Scalar(v) => vec![v.clone()],
        Message::Array(v) => v.clone(),
        Message::Map(entries) => entries.iter().map(|e| e.value.clone()).collect(),
        Message::ShapedObject { values, .. } => values.clone(),
        Message::SchemaObject { fields, .. } => fields.clone(),
        Message::TypedVector(v) => match &v.data {
            TypedVectorData::Bool(d) => d.iter().copied().map(Value::Bool).collect(),
            TypedVectorData::I64(d) => d.iter().copied().map(Value::I64).collect(),
            TypedVectorData::U64(d) => d.iter().copied().map(Value::U64).collect(),
            TypedVectorData::F64(d) => d.iter().copied().map(Value::F64).collect(),
            TypedVectorData::String(d) => d.iter().cloned().map(Value::String).collect(),
            TypedVectorData::Binary(d) => d.iter().cloned().map(Value::Binary).collect(),
            TypedVectorData::Value(d) => d.clone(),
        },
        Message::RowBatch { rows } => rows.iter().flat_map(|r| r.clone()).collect(),
        Message::ColumnBatch { columns, .. } => columns
            .iter()
            .flat_map(|c| match &c.values {
                TypedVectorData::Bool(v) => v.iter().copied().map(Value::Bool).collect(),
                TypedVectorData::I64(v) => v.iter().copied().map(Value::I64).collect(),
                TypedVectorData::U64(v) => v.iter().copied().map(Value::U64).collect(),
                TypedVectorData::F64(v) => v.iter().copied().map(Value::F64).collect(),
                TypedVectorData::String(v) => v.iter().cloned().map(Value::String).collect(),
                TypedVectorData::Binary(v) => v.iter().cloned().map(Value::Binary).collect(),
                TypedVectorData::Value(v) => v.clone(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn rebuild_message_like(base: &Message, fields: Vec<Value>) -> Result<Message> {
    match base {
        Message::Scalar(_) => Ok(Message::Scalar(
            fields.into_iter().next().unwrap_or(Value::Null),
        )),
        Message::Array(_) => Ok(Message::Array(fields)),
        Message::Map(entries) => {
            if fields.len() != entries.len() {
                return Err(RecurramError::InvalidData(
                    "patch map field count mismatch (insert/delete unsupported)",
                ));
            }
            let mut out = Vec::with_capacity(entries.len());
            for (entry, value) in entries.iter().zip(fields) {
                out.push(MapEntry {
                    key: entry.key.clone(),
                    value,
                });
            }
            Ok(Message::Map(out))
        }
        Message::ShapedObject {
            shape_id, presence, ..
        } => Ok(Message::ShapedObject {
            shape_id: *shape_id,
            presence: presence.clone(),
            values: fields,
        }),
        Message::SchemaObject {
            schema_id,
            presence,
            ..
        } => Ok(Message::SchemaObject {
            schema_id: *schema_id,
            presence: presence.clone(),
            fields,
        }),
        Message::TypedVector(vector) => {
            let data = match vector.element_type {
                ElementType::Bool => TypedVectorData::Bool(
                    fields
                        .iter()
                        .map(|v| match v {
                            Value::Bool(b) => Ok(*b),
                            _ => Err(RecurramError::InvalidData("typed bool patch")),
                        })
                        .collect::<Result<Vec<_>>>()?,
                ),
                ElementType::I64 => TypedVectorData::I64(
                    fields
                        .iter()
                        .map(|v| match v {
                            Value::I64(i) => Ok(*i),
                            _ => Err(RecurramError::InvalidData("typed i64 patch")),
                        })
                        .collect::<Result<Vec<_>>>()?,
                ),
                ElementType::U64 => TypedVectorData::U64(
                    fields
                        .iter()
                        .map(|v| match v {
                            Value::U64(i) => Ok(*i),
                            _ => Err(RecurramError::InvalidData("typed u64 patch")),
                        })
                        .collect::<Result<Vec<_>>>()?,
                ),
                ElementType::F64 => TypedVectorData::F64(
                    fields
                        .iter()
                        .map(|v| match v {
                            Value::F64(f) => Ok(*f),
                            _ => Err(RecurramError::InvalidData("typed f64 patch")),
                        })
                        .collect::<Result<Vec<_>>>()?,
                ),
                ElementType::String => TypedVectorData::String(
                    fields
                        .iter()
                        .map(|v| match v {
                            Value::String(s) => Ok(s.clone()),
                            _ => Err(RecurramError::InvalidData("typed string patch")),
                        })
                        .collect::<Result<Vec<_>>>()?,
                ),
                ElementType::Binary => TypedVectorData::Binary(
                    fields
                        .iter()
                        .map(|v| match v {
                            Value::Binary(b) => Ok(b.clone()),
                            _ => Err(RecurramError::InvalidData("typed binary patch")),
                        })
                        .collect::<Result<Vec<_>>>()?,
                ),
                ElementType::Value => TypedVectorData::Value(fields),
            };
            Ok(Message::TypedVector(TypedVector {
                element_type: vector.element_type,
                codec: vector.codec,
                data,
            }))
        }
        Message::RowBatch { .. }
        | Message::ColumnBatch { .. }
        | Message::Control(_)
        | Message::Ext { .. }
        | Message::StatePatch { .. }
        | Message::TemplateBatch { .. }
        | Message::ControlStream { .. }
        | Message::BaseSnapshot { .. } => Err(RecurramError::InvalidData(
            "state patch reconstruction unsupported for this message kind",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_template_id_prefers_lowest_matching_id() {
        let mut templates = std::collections::HashMap::new();
        let descriptor_2 = TemplateDescriptor {
            template_id: 2,
            field_ids: vec![0, 1],
            null_strategies: vec![
                NullStrategy::AllPresentElided,
                NullStrategy::AllPresentElided,
            ],
            codecs: vec![VectorCodec::Plain, VectorCodec::Dictionary],
        };
        let descriptor_5 = TemplateDescriptor {
            template_id: 5,
            field_ids: vec![0, 1],
            null_strategies: vec![
                NullStrategy::AllPresentElided,
                NullStrategy::AllPresentElided,
            ],
            codecs: vec![VectorCodec::Plain, VectorCodec::Dictionary],
        };
        templates.insert(5, descriptor_5);
        templates.insert(2, descriptor_2);

        let probe = TemplateDescriptor {
            template_id: 0,
            field_ids: vec![0, 1],
            null_strategies: vec![
                NullStrategy::AllPresentElided,
                NullStrategy::AllPresentElided,
            ],
            codecs: vec![VectorCodec::Plain, VectorCodec::Dictionary],
        };

        assert_eq!(find_template_id(&templates, &probe), Some(2));
    }
}
