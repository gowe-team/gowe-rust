pub mod codec;
pub mod error;
pub mod model;
pub mod protocol;
pub mod session;
pub mod v2;
pub mod wire;

pub use error::{RecurramError, Result};
pub use model::{Message, Schema, Value};
pub use protocol::{RecurramCodec, SessionEncoder};
pub use session::{SessionOptions, UnknownReferencePolicy};

pub fn encode(value: &Value) -> Result<Vec<u8>> {
    v2::encode(value)
}

pub fn decode(bytes: &[u8]) -> Result<Value> {
    v2::decode(bytes)
}

pub fn encode_with_schema(schema: &Schema, value: &Value) -> Result<Vec<u8>> {
    SessionEncoder::new(SessionOptions::default()).encode_with_schema(schema, value)
}

pub fn encode_batch(values: &[Value]) -> Result<Vec<u8>> {
    SessionEncoder::new(SessionOptions::default()).encode_batch(values)
}

pub fn create_session_encoder(options: SessionOptions) -> SessionEncoder {
    SessionEncoder::new(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        BaseRef, Column, ControlMessage, ControlStreamCodec, ElementType, Message, NullStrategy,
        PatchOpcode, PatchOperation, TypedVector, TypedVectorData, VectorCodec,
    };
    use crate::session::UnknownReferencePolicy;

    #[test]
    fn roundtrip_dynamic_value() {
        let value = Value::Map(vec![
            ("id".to_string(), Value::U64(1001)),
            ("name".to_string(), Value::String("alice".to_string())),
            ("admin".to_string(), Value::Bool(false)),
            (
                "scores".to_string(),
                Value::Array(vec![
                    Value::I64(12),
                    Value::I64(15),
                    Value::I64(18),
                    Value::I64(21),
                ]),
            ),
        ]);
        let mut codec = RecurramCodec::default();
        let encoded = codec.encode_value(&value).expect("encode");
        let decoded = codec.decode_value(&encoded).expect("decode");
        assert_eq!(decoded, value);
    }

    #[test]
    fn roundtrip_all_message_kinds() {
        let mut codec = RecurramCodec::default();
        codec.state.previous_message = Some(Message::Scalar(Value::U64(0)));
        let messages = vec![
            Message::Scalar(Value::U64(1)),
            Message::Array(vec![Value::Bool(true), Value::I64(3)]),
            Message::Map(vec![crate::model::MapEntry {
                key: crate::model::KeyRef::Literal("id".to_string()),
                value: Value::U64(7),
            }]),
            Message::ShapedObject {
                shape_id: codec.state.shape_table.register(vec!["id".to_string()]),
                presence: None,
                values: vec![Value::U64(8)],
            },
            Message::SchemaObject {
                schema_id: Some(42),
                presence: None,
                fields: vec![Value::U64(9)],
            },
            Message::TypedVector(TypedVector {
                element_type: ElementType::I64,
                codec: VectorCodec::DeltaDeltaBitpack,
                data: TypedVectorData::I64(vec![10, 20, 30, 40]),
            }),
            Message::RowBatch {
                rows: vec![vec![Value::U64(1)], vec![Value::U64(2)]],
            },
            Message::ColumnBatch {
                count: 2,
                columns: vec![Column {
                    field_id: 1,
                    null_strategy: NullStrategy::PresenceBitmap,
                    presence: Some(vec![true, true]),
                    codec: VectorCodec::Plain,
                    dictionary_id: None,
                    values: TypedVectorData::Value(vec![Value::U64(1), Value::U64(2)]),
                }],
            },
            Message::Control(ControlMessage::RegisterKeys(vec!["id".to_string()])),
            Message::Ext {
                ext_type: 1,
                payload: vec![1, 2, 3],
            },
            Message::StatePatch {
                base_ref: BaseRef::Previous,
                operations: vec![PatchOperation {
                    field_id: 0,
                    opcode: PatchOpcode::Keep,
                    value: None,
                }],
                literals: vec![],
            },
            Message::TemplateBatch {
                template_id: 1,
                count: 1,
                changed_column_mask: vec![true],
                columns: vec![Column {
                    field_id: 0,
                    null_strategy: NullStrategy::PresenceBitmap,
                    presence: Some(vec![true]),
                    codec: VectorCodec::Plain,
                    dictionary_id: None,
                    values: TypedVectorData::Value(vec![Value::U64(1)]),
                }],
            },
            Message::ControlStream {
                codec: ControlStreamCodec::Rle,
                payload: vec![0, 1],
            },
            Message::BaseSnapshot {
                base_id: 1,
                schema_or_shape_ref: 0,
                payload: Box::new(Message::Scalar(Value::U64(10))),
            },
        ];

        for msg in messages {
            let encoded = codec.encode_message(&msg).expect("encode message");
            let decoded = codec.decode_message(&encoded).expect("decode message");
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn session_patch_and_micro_batch() {
        let mut enc = create_session_encoder(SessionOptions::default());
        let base = Value::Map(vec![
            ("id".to_string(), Value::U64(1)),
            ("name".to_string(), Value::String("alice".to_string())),
        ]);
        let next = Value::Map(vec![
            ("id".to_string(), Value::U64(1)),
            ("name".to_string(), Value::String("alicia".to_string())),
        ]);
        let _ = enc.encode(&base).expect("encode base");
        let patch = enc.encode_patch(&next).expect("encode patch");
        assert!(!patch.is_empty());

        let micro = enc
            .encode_micro_batch(&[base.clone(), next.clone(), base.clone(), next.clone()])
            .expect("encode micro");
        assert!(!micro.is_empty());
    }

    #[test]
    fn codec_selection_uses_delta_delta_for_regular_series() {
        let value = Value::Array((0..16).map(|i| Value::I64(1_000 + (i * 10))).collect());
        let mut codec = RecurramCodec::default();
        let bytes = codec.encode_value(&value).expect("encode");
        let msg = codec.decode_message(&bytes).expect("decode message");
        match msg {
            Message::TypedVector(v) => assert_eq!(v.codec, VectorCodec::DeltaDeltaBitpack),
            other => panic!("expected typed vector, got {other:?}"),
        }
    }

    #[test]
    fn unknown_reference_policy_supports_stateless_retry() {
        let options = SessionOptions {
            unknown_reference_policy: UnknownReferencePolicy::StatelessRetry,
            ..SessionOptions::default()
        };
        let mut codec = RecurramCodec::with_options(options);
        let patch = Message::StatePatch {
            base_ref: BaseRef::BaseId(777),
            operations: vec![],
            literals: vec![],
        };
        let mut raw = Vec::new();
        raw.extend(codec.encode_message(&patch).expect("encode"));

        let mut decode_codec = RecurramCodec::with_options(SessionOptions {
            unknown_reference_policy: UnknownReferencePolicy::StatelessRetry,
            ..SessionOptions::default()
        });
        let err = decode_codec
            .decode_message(&raw)
            .expect_err("expected retry error");
        match err {
            RecurramError::StatelessRetryRequired("base_id", 777) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn patch_selection_uses_previous_base_for_safe_interop() {
        let mut enc = create_session_encoder(SessionOptions::default());
        let base = Value::Map(vec![
            ("id".to_string(), Value::U64(1)),
            ("name".to_string(), Value::String("alice".to_string())),
            ("score".to_string(), Value::I64(100)),
        ]);
        let unrelated = Value::Map(vec![
            ("id".to_string(), Value::U64(999)),
            ("name".to_string(), Value::String("zeta".to_string())),
            ("score".to_string(), Value::I64(-500)),
        ]);
        let near_base = Value::Map(vec![
            ("id".to_string(), Value::U64(1)),
            ("name".to_string(), Value::String("alice".to_string())),
            ("score".to_string(), Value::I64(101)),
        ]);

        let _ = enc.encode(&base).expect("encode base");
        let _ = enc.encode(&unrelated).expect("encode unrelated");
        let patch_bytes = enc.encode_patch(&near_base).expect("encode patch");
        let decoded = enc.decode_message(&patch_bytes).expect("decode patch");
        match decoded {
            Message::StatePatch { base_ref, .. } => match base_ref {
                BaseRef::Previous => {}
                other => panic!("expected previous-message patch, got {other:?}"),
            },
            Message::Map(_) | Message::ShapedObject { .. } => {}
            other => panic!("unexpected message kind: {other:?}"),
        }
    }
}
