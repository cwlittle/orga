use crate::{
    encoding::{Decode, Encode},
    state::State,
    store::Store,
    Error, Result,
};
use js_sys::{Array, Uint8Array};
use std::{
    any::Any,
    fmt::{Debug, Display},
    ops::Deref,
};
use wasm_bindgen::prelude::*;

mod builder;

pub use builder::Builder;

pub trait Describe {
    fn describe() -> Descriptor;
}

#[wasm_bindgen(getter_with_clone, inspectable)]
#[derive(Clone)]
pub struct Descriptor {
    pub type_name: String,
    children: Children,
    decode: DecodeFn,
    parse: ParseFn,
}

impl Debug for Descriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Descriptor")
            .field("type_name", &self.type_name)
            .field("children", &self.children)
            .finish()
    }
}

impl Descriptor {
    pub fn decode(&self, bytes: &[u8]) -> Result<Value> {
        (self.decode)(bytes)
    }

    pub fn from_str(&self, string: &str) -> Result<Option<Value>> {
        (self.parse)(string)
    }
}

#[wasm_bindgen]
impl Descriptor {
    #[wasm_bindgen(js_name = children)]
    pub fn children_js(&self) -> JsValue {
        match &self.children {
            Children::None => JsValue::NULL,
            Children::Named(children) => children
                .iter()
                .cloned()
                .map(JsValue::from)
                .collect::<Array>()
                .into(),
            Children::Dynamic(child) => child.clone().into(),
        }
    }

    #[wasm_bindgen(js_name = decode)]
    pub fn decode_js(&self, bytes: js_sys::Uint8Array) -> Value {
        // TODO: return Result
        self.decode(bytes.to_vec().as_slice()).unwrap()
    }
}

pub type DecodeFn = fn(&[u8]) -> Result<Value>;
pub type ParseFn = fn(&str) -> Result<Option<Value>>;

#[derive(Clone, Debug)]
pub enum Children {
    None,
    Named(Vec<NamedChild>),
    Dynamic(DynamicChild),
}

impl Default for Children {
    fn default() -> Self {
        Children::None
    }
}

#[wasm_bindgen(getter_with_clone, inspectable)]
#[derive(Clone)]
pub struct NamedChild {
    pub name: String,
    pub desc: Descriptor,
    store_key: KeyOp,
    access: AccessFn,
}

pub type AccessFn = fn(&Value) -> Result<Value>;

impl Debug for NamedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NamedChild")
            .field("name", &self.name)
            .field("desc", &self.desc)
            .field("store_key", &self.store_key)
            .finish()
    }
}

#[wasm_bindgen(inspectable)]
#[derive(Clone, Debug)]
pub struct DynamicChild {
    key_desc: Box<Descriptor>,
    value_desc: Box<Descriptor>,
}

#[derive(Clone, Debug)]
pub enum KeyOp {
    Append(Vec<u8>),
    Absolute(Vec<u8>),
}

#[wasm_bindgen]
pub struct WrappedStore(Store);

#[wasm_bindgen]
pub struct Value {
    instance: Box<dyn Inspect>,
    store: Store,
}

impl Value {
    pub fn new<T: Inspect + 'static>(instance: T) -> Self {
        Value {
            instance: Box::new(instance),
            store: Store::default(),
        }
    }

    pub fn attach(&mut self, store: Store) -> Result<()> {
        self.store = store.clone();
        self.instance.attach(store)
    }

    pub fn downcast<T: Inspect + 'static>(&self) -> Option<T> {
        let any = self.instance.to_any().unwrap();
        match any.downcast::<T>() {
            Ok(mut boxed) => {
                // TODO: return Result
                boxed.attach(self.store.clone()).unwrap();
                Some(*boxed)
            }
            Err(_) => None,
        }
    }

    pub fn child(&self, name: &str) -> Result<Value> {
        let desc = self.describe();
        match desc.children {
            Children::None => Err(Error::Downcast("Value does not have children".to_string())),
            Children::Named(children) => {
                let cdesc = children
                    .iter()
                    .find(|c| c.name == name)
                    .ok_or_else(|| Error::Downcast(format!("No child called '{}'", name)))?;
                let substore = match &cdesc.store_key {
                    KeyOp::Absolute(prefix) => unsafe { self.store.with_prefix(prefix.clone()) },
                    KeyOp::Append(prefix) => self.store.sub(prefix.as_slice()),
                };
                let mut child = (cdesc.access)(self)?;
                child.attach(substore)?;
                Ok(child)
            }
            Children::Dynamic(child) => {
                // TODO: fix unwraps
                let key = child.key_desc.from_str(name)?.unwrap();
                let key_bytes = key.encode()?;
                use crate::store::Read;
                let value_bytes = self.store.get(key_bytes.as_slice())?.unwrap();
                let value = child.value_desc.decode(value_bytes.as_slice())?;
                Ok(value)
            }
        }
    }
}

#[wasm_bindgen]
impl Value {
    #[wasm_bindgen(js_name = toString)]
    pub fn to_string_js(&self) -> Option<String> {
        self.maybe_to_string()
    }

    #[wasm_bindgen(js_name = debug)]
    pub fn maybe_debug_js(&self, alternate: Option<bool>) -> Option<String> {
        let alternate = alternate.unwrap_or_default();
        self.maybe_debug(alternate)
    }

    #[wasm_bindgen(js_name = child)]
    pub fn child_js(&self, name: &str) -> Value {
        // TODO: return Result
        self.child(name).unwrap()
    }

    #[wasm_bindgen(js_name = encode)]
    pub fn encode_js(&self) -> Uint8Array {
        // TODO: return Result
        self.encode().unwrap().as_slice().into()
    }
}

impl Deref for Value {
    type Target = dyn Inspect;

    fn deref(&self) -> &Self::Target {
        &*self.instance
    }
}

pub trait Inspect {
    fn maybe_to_string(&self) -> Option<String> {
        MaybeDisplay::maybe_to_string(&DisplayWrapper(&self))
    }

    fn maybe_debug(&self, alternate: bool) -> Option<String> {
        MaybeDebug::maybe_debug(&DebugWrapper(&self), alternate)
    }

    // TODO: should this be a maybe impl?
    fn encode(&self) -> Result<Vec<u8>>;

    // TODO: should this be a maybe impl?
    fn describe(&self) -> Descriptor;

    // TODO: should this be a maybe impl?
    fn attach(&mut self, store: Store) -> Result<()>;

    fn to_any(&self) -> Result<Box<dyn Any>>;

    // TODO: maybe_to_object
    // TODO: query
    // TODO: call
}

impl<T: State + Describe + 'static> Inspect for T {
    fn encode(&self) -> Result<Vec<u8>> {
        Ok(Encode::encode(self)?)
    }

    fn describe(&self) -> Descriptor {
        Self::describe()
    }

    fn attach(&mut self, store: Store) -> Result<()> {
        State::attach(self, store)
    }

    fn to_any(&self) -> Result<Box<dyn Any>> {
        let bytes = self.encode()?;
        let cloned = Self::decode(bytes.as_slice())?;
        Ok(Box::new(cloned))
    }
}

trait MaybeDisplay {
    fn maybe_to_string(&self) -> Option<String>;
}

struct DisplayWrapper<'a, T>(&'a T);

impl<'a, T> MaybeDisplay for DisplayWrapper<'a, T> {
    default fn maybe_to_string(&self) -> Option<String> {
        None
    }
}

impl<'a, T: Display> MaybeDisplay for DisplayWrapper<'a, T> {
    fn maybe_to_string(&self) -> Option<String> {
        Some(format!("{}", self.0))
    }
}

trait MaybeDebug {
    fn maybe_debug(&self, alternate: bool) -> Option<String>;
}

struct DebugWrapper<'a, T>(&'a T);

impl<'a, T> MaybeDebug for DebugWrapper<'a, T> {
    default fn maybe_debug(&self, _: bool) -> Option<String> {
        None
    }
}

impl<'a, T: Debug> MaybeDebug for DebugWrapper<'a, T> {
    fn maybe_debug(&self, alternate: bool) -> Option<String> {
        Some(if alternate {
            format!("{:#?}", self.0)
        } else {
            format!("{:?}", self.0)
        })
    }
}

macro_rules! primitive_impl {
    ($ty:ty) => {
        impl Describe for $ty {
            fn describe() -> Descriptor {
                Builder::new::<Self>().build()
            }
        }
    };
}

primitive_impl!(u8);
primitive_impl!(u16);
primitive_impl!(u32);
primitive_impl!(u64);
primitive_impl!(u128);
primitive_impl!(i8);
primitive_impl!(i16);
primitive_impl!(i32);
primitive_impl!(i64);
primitive_impl!(i128);
primitive_impl!(bool);
primitive_impl!(());

#[cfg(test)]
mod tests {
    use super::{Builder, Describe, Descriptor, Value};
    use crate::{
        collections::Map,
        encoding::{Decode, Encode},
        state::State,
        store::{DefaultBackingStore, MapStore, Shared, Store},
    };

    #[derive(State, Encode, Decode, Debug)]
    struct Foo {
        bar: u32,
        baz: u32,
    }

    impl Describe for Foo {
        fn describe() -> Descriptor {
            Builder::new::<Self>()
                .named_child::<u32>("bar", &[0], |v| Builder::access(v, |v: Self| v.bar))
                .named_child::<u32>("baz", &[1], |v| Builder::access(v, |v: Self| v.baz))
                .build()
        }
    }

    #[derive(State, Encode, Decode, Default)]
    struct Bar {
        bar: u32,
        baz: Map<u32, u32>,
    }

    impl Describe for Bar {
        fn describe() -> Descriptor {
            Builder::new::<Self>()
                .named_child::<u32>("bar", &[0], |v| Builder::access(v, |v: Self| v.bar))
                .named_child::<Map<u32, u32>>("baz", &[1], |v| Builder::access(v, |v: Self| v.baz))
                .build()
        }
    }

    #[test]
    fn decode() {
        let desc = Foo::describe();
        let value = desc.decode(&[0, 0, 1, 164, 0, 0, 0, 69]).unwrap();
        assert_eq!(
            value.maybe_debug(false).unwrap(),
            "Foo { bar: 420, baz: 69 }"
        );
    }

    #[test]
    fn downcast() {
        let value = Value::new(Foo { bar: 420, baz: 69 });
        let foo: Foo = value.downcast().unwrap();
        assert_eq!(foo.bar, 420);
        assert_eq!(foo.baz, 69);
    }

    #[test]
    fn child() {
        let value = Value::new(Foo { bar: 420, baz: 69 });
        let bar: u32 = value.child("bar").unwrap().downcast().unwrap();
        let baz: u32 = value.child("baz").unwrap().downcast().unwrap();
        assert_eq!(bar, 420);
        assert_eq!(baz, 69);
    }

    #[test]
    fn complex_child() {
        let store = Store::new(DefaultBackingStore::MapStore(Shared::new(MapStore::new())));
        let mut bar = Bar::default();
        bar.attach(store.clone()).unwrap();
        bar.baz.insert(123, 456).unwrap();
        bar.flush().unwrap();

        let mut value = Value::new(bar);
        value.attach(store).unwrap();

        let baz = value.child("baz").unwrap();
        assert_eq!(baz.child("123").unwrap().downcast::<u32>().unwrap(), 456);
    }
}
