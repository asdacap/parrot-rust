//! Reflection internals.

use std::marker;

use crate::reflect::runtime_types::RuntimeTypeTrait;
use crate::reflect::types::ProtobufTypeTrait;
use crate::reflect::ProtobufValue;
use crate::reflect::RuntimeType;

/// Dynamic version of [`ProtobufType`](crate::reflect::types::ProtobufType).
///
/// This is used internally.
pub(crate) trait _ProtobufTypeDynamic: Send + Sync + 'static {
    /// Get runtime type for this protobuf type.
    fn runtime_type(&self) -> RuntimeType;
}

pub(crate) struct _ProtobufTypeDynamicImpl<T: ProtobufTypeTrait>(pub marker::PhantomData<T>);

impl<T> _ProtobufTypeDynamic for _ProtobufTypeDynamicImpl<T>
where
    T: ProtobufTypeTrait,
    <T as ProtobufTypeTrait>::ProtobufValue: ProtobufValue,
{
    fn runtime_type(&self) -> RuntimeType {
        <T::ProtobufValue as ProtobufValue>::RuntimeType::runtime_type_box()
    }
}
