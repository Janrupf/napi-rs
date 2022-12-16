use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::ptr;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use once_cell::sync::Lazy;

use crate::{
  check_status, check_status_or_throw, sys, Env, JsError, JsFunction, Property, Result, Value,
  ValueType,
};

pub type ExportRegisterCallback = unsafe fn(sys::napi_env) -> Result<sys::napi_value>;
pub type ModuleExportsCallback =
  unsafe fn(env: sys::napi_env, exports: sys::napi_value) -> Result<()>;

struct PersistedPerInstanceVec<T> {
  inner: AtomicPtr<T>,
  length: AtomicUsize,
}

impl<T> Default for PersistedPerInstanceVec<T> {
  fn default() -> Self {
    let mut vec: Vec<T> = Vec::with_capacity(1);
    let ret = Self {
      inner: AtomicPtr::new(vec.as_mut_ptr()),
      length: AtomicUsize::new(0),
    };
    std::mem::forget(vec);
    ret
  }
}

impl<T> PersistedPerInstanceVec<T> {
  #[allow(clippy::mut_from_ref)]
  fn borrow_mut<F>(&self, f: F)
  where
    F: FnOnce(&mut [T]),
  {
    let length = self.length.load(Ordering::Relaxed);
    if length == 0 {
      f(&mut []);
    } else {
      let inner = self.inner.load(Ordering::Relaxed);
      let mut temp = unsafe { Vec::from_raw_parts(inner, length, length) };
      f(temp.as_mut_slice());
      // Inner Vec has been reallocated, so we need to update the pointer
      if temp.as_mut_ptr() != inner {
        self.inner.store(temp.as_mut_ptr(), Ordering::Relaxed);
      }
      self.length.store(temp.len(), Ordering::Relaxed);
      std::mem::forget(temp);
    }
  }

  fn push(&self, item: T) {
    let length = self.length.load(Ordering::Relaxed);
    let inner = self.inner.load(Ordering::Relaxed);
    let mut temp = unsafe { Vec::from_raw_parts(inner, length, length) };
    temp.push(item);
    // Inner Vec has been reallocated, so we need to update the pointer
    if temp.as_mut_ptr() != inner {
      self.inner.store(temp.as_mut_ptr(), Ordering::Relaxed);
    }
    std::mem::forget(temp);

    self.length.fetch_add(1, Ordering::Relaxed);
  }
}

unsafe impl<T: Send> Send for PersistedPerInstanceVec<T> {}
unsafe impl<T: Sync> Sync for PersistedPerInstanceVec<T> {}

pub(crate) struct PersistedPerInstanceHashMap<K, V>(*mut HashMap<K, V>);

impl<K, V> PersistedPerInstanceHashMap<K, V> {
  #[allow(clippy::mut_from_ref)]
  pub(crate) fn borrow_mut<F, R>(&self, f: F) -> R
  where
    F: FnOnce(&mut HashMap<K, V>) -> R,
  {
    f(unsafe { Box::leak(Box::from_raw(self.0)) })
  }
}

impl<K, V> Default for PersistedPerInstanceHashMap<K, V> {
  fn default() -> Self {
    let map = Default::default();
    Self(Box::into_raw(Box::new(map)))
  }
}

type ModuleRegisterCallback =
  PersistedPerInstanceVec<(Option<&'static str>, (&'static str, ExportRegisterCallback))>;

type ModuleClassProperty = PersistedPerInstanceHashMap<
  &'static str,
  HashMap<Option<&'static str>, (&'static str, Vec<Property>)>,
>;

unsafe impl<K, V> Send for PersistedPerInstanceHashMap<K, V> {}
unsafe impl<K, V> Sync for PersistedPerInstanceHashMap<K, V> {}

type FnRegisterMap =
  PersistedPerInstanceHashMap<ExportRegisterCallback, (sys::napi_callback, &'static str)>;

static MODULE_REGISTER_CALLBACK: Lazy<ModuleRegisterCallback> = Lazy::new(Default::default);
static MODULE_CLASS_PROPERTIES: Lazy<ModuleClassProperty> = Lazy::new(Default::default);
static REGISTERED: AtomicBool = AtomicBool::new(false);
static REGISTERED_CLASSES: Lazy<thread_local::ThreadLocal<AtomicPtr<RegisteredClasses>>> =
  Lazy::new(thread_local::ThreadLocal::new);
static FN_REGISTER_MAP: Lazy<FnRegisterMap> = Lazy::new(Default::default);

#[ctor::dtor]
fn destroy() {
  {
    let ptr = MODULE_REGISTER_CALLBACK.inner.load(Ordering::Relaxed);
    let len = MODULE_REGISTER_CALLBACK.length.load(Ordering::Relaxed);
    unsafe { Vec::from_raw_parts(ptr, len, len) };
  }
  {
    unsafe { Box::from_raw(MODULE_CLASS_PROPERTIES.0) };
  }
  {
    unsafe { Box::from_raw(FN_REGISTER_MAP.0) };
  }
}

#[inline]
fn wait_first_thread_registered() {
  while !REGISTERED.load(Ordering::SeqCst) {
    std::hint::spin_loop();
  }
}

type RegisteredClasses =
  HashMap</* export name */ String, /* constructor */ sys::napi_ref>;

#[cfg(feature = "compat-mode")]
// compatibility for #[module_exports]

static MODULE_EXPORTS: Lazy<PersistedPerInstanceVec<ModuleExportsCallback>> =
  Lazy::new(Default::default);

#[doc(hidden)]
pub fn get_class_constructor(js_name: &'static str) -> Option<sys::napi_ref> {
  wait_first_thread_registered();
  let registered_classes = REGISTERED_CLASSES.get().unwrap();
  let registered_classes =
    Box::leak(unsafe { Box::from_raw(registered_classes.load(Ordering::Relaxed)) });
  registered_classes.get(js_name).copied()
}

#[doc(hidden)]
#[cfg(feature = "compat-mode")]
// compatibility for #[module_exports]
pub fn register_module_exports(callback: ModuleExportsCallback) {
  MODULE_EXPORTS.push(callback);
}

#[doc(hidden)]
pub fn register_module_export(
  js_mod: Option<&'static str>,
  name: &'static str,
  cb: ExportRegisterCallback,
) {
  MODULE_REGISTER_CALLBACK.push((js_mod, (name, cb)));
}

#[doc(hidden)]
pub fn register_js_function(
  name: &'static str,
  cb: ExportRegisterCallback,
  c_fn: sys::napi_callback,
) {
  FN_REGISTER_MAP.borrow_mut(|inner| {
    inner.insert(cb, (c_fn, name));
  });
}

#[doc(hidden)]
pub fn register_class(
  rust_name: &'static str,
  js_mod: Option<&'static str>,
  js_name: &'static str,
  props: Vec<Property>,
) {
  MODULE_CLASS_PROPERTIES.borrow_mut(|inner| {
    let val = inner.entry(rust_name).or_default();
    let val = val.entry(js_mod).or_default();
    val.0 = js_name;
    val.1.extend(props.into_iter());
  });
}

#[inline]
/// Get `JsFunction` from defined Rust `fn`
/// ```rust
/// #[napi]
/// fn some_fn() -> u32 {
///     1
/// }
///
/// #[napi]
/// fn return_some_fn() -> Result<JsFunction> {
///     get_js_function(some_fn_js_function)
/// }
/// ```
///
/// ```js
/// returnSomeFn()(); // 1
/// ```
///
pub fn get_js_function(env: &Env, raw_fn: ExportRegisterCallback) -> Result<JsFunction> {
  wait_first_thread_registered();
  FN_REGISTER_MAP.borrow_mut(|inner| {
    inner
      .get(&raw_fn)
      .and_then(|(cb, name)| {
        let mut function = ptr::null_mut();
        let name_len = name.len() - 1;
        let fn_name = unsafe { CStr::from_bytes_with_nul_unchecked(name.as_bytes()) };
        check_status!(unsafe {
          sys::napi_create_function(
            env.0,
            fn_name.as_ptr(),
            name_len,
            *cb,
            ptr::null_mut(),
            &mut function,
          )
        })
        .ok()?;
        Some(JsFunction(Value {
          env: env.0,
          value: function,
          value_type: ValueType::Function,
        }))
      })
      .ok_or_else(|| {
        crate::Error::new(
          crate::Status::InvalidArg,
          "JavaScript function does not exist".to_owned(),
        )
      })
  })
}

/// Get `C Callback` from defined Rust `fn`
/// ```rust
/// #[napi]
/// fn some_fn() -> u32 {
///     1
/// }
///
/// #[napi]
/// fn create_obj(env: Env) -> Result<JsObject> {
///     let mut obj = env.create_object()?;
///     obj.define_property(&[Property::new("getter")?.with_getter(get_c_callback(some_fn_js_function)?)])?;
///     Ok(obj)
/// }
/// ```
///
/// ```js
/// console.log(createObj().getter) // 1
/// ```
///
pub fn get_c_callback(raw_fn: ExportRegisterCallback) -> Result<crate::Callback> {
  wait_first_thread_registered();
  FN_REGISTER_MAP.borrow_mut(|inner| {
    inner
      .get(&raw_fn)
      .and_then(|(cb, _name)| *cb)
      .ok_or_else(|| {
        crate::Error::new(
          crate::Status::InvalidArg,
          "JavaScript function does not exist".to_owned(),
        )
      })
  })
}

#[cfg(windows)]
#[ctor::ctor]
fn load_host() {
  unsafe {
    sys::setup();
  }
}

#[no_mangle]
unsafe extern "C" fn napi_register_module_v1(
  env: sys::napi_env,
  exports: sys::napi_value,
) -> sys::napi_value {
  crate::__private::___CALL_FROM_FACTORY.get_or_default();
  let registered_classes_ptr = REGISTERED_CLASSES.get_or_default();
  let mut exports_objects: HashSet<String> = HashSet::default();
  MODULE_REGISTER_CALLBACK.borrow_mut(|inner| {
    inner
      .iter_mut()
      .fold(
        HashMap::<Option<&'static str>, Vec<(&'static str, ExportRegisterCallback)>>::new(),
        |mut acc, (js_mod, item)| {
          if let Some(k) = acc.get_mut(js_mod) {
            k.push(*item);
          } else {
            acc.insert(*js_mod, vec![*item]);
          }
          acc
        },
      )
      .iter()
      .for_each(|(js_mod, items)| {
        let mut exports_js_mod = ptr::null_mut();
        if let Some(js_mod_str) = js_mod {
          let mod_name_c_str =
            unsafe { CStr::from_bytes_with_nul_unchecked(js_mod_str.as_bytes()) };
          if exports_objects.contains(*js_mod_str) {
            check_status_or_throw!(
              env,
              unsafe {
                sys::napi_get_named_property(
                  env,
                  exports,
                  mod_name_c_str.as_ptr(),
                  &mut exports_js_mod,
                )
              },
              "Get mod {} from exports failed",
              js_mod_str,
            );
          } else {
            check_status_or_throw!(
              env,
              unsafe { sys::napi_create_object(env, &mut exports_js_mod) },
              "Create export JavaScript Object [{}] failed",
              js_mod_str
            );
            check_status_or_throw!(
              env,
              unsafe {
                sys::napi_set_named_property(env, exports, mod_name_c_str.as_ptr(), exports_js_mod)
              },
              "Set exports Object [{}] into exports object failed",
              js_mod_str
            );
            exports_objects.insert(js_mod_str.to_string());
          }
        }
        for (name, callback) in items {
          unsafe {
            let js_name = CStr::from_bytes_with_nul_unchecked(name.as_bytes());
            if let Err(e) = callback(env).and_then(|v| {
              let exported_object = if exports_js_mod.is_null() {
                exports
              } else {
                exports_js_mod
              };
              check_status!(
                sys::napi_set_named_property(env, exported_object, js_name.as_ptr(), v),
                "Failed to register export `{}`",
                name,
              )
            }) {
              JsError::from(e).throw_into(env)
            }
          }
        }
      })
  });

  let mut registered_classes: RegisteredClasses =
    HashMap::with_capacity(MODULE_CLASS_PROPERTIES.borrow_mut(|inner| inner.len()));

  MODULE_CLASS_PROPERTIES.borrow_mut(|inner| {
    inner.iter().for_each(|(rust_name, js_mods)| {
      for (js_mod, (js_name, props)) in js_mods {
        let mut exports_js_mod = ptr::null_mut();
        unsafe {
          if let Some(js_mod_str) = js_mod {
            let mod_name_c_str = CStr::from_bytes_with_nul_unchecked(js_mod_str.as_bytes());
            if exports_objects.contains(*js_mod_str) {
              check_status_or_throw!(
                env,
                sys::napi_get_named_property(
                  env,
                  exports,
                  mod_name_c_str.as_ptr(),
                  &mut exports_js_mod,
                ),
                "Get mod {} from exports failed",
                js_mod_str,
              );
            } else {
              check_status_or_throw!(
                env,
                sys::napi_create_object(env, &mut exports_js_mod),
                "Create export JavaScript Object [{}] failed",
                js_mod_str
              );
              check_status_or_throw!(
                env,
                sys::napi_set_named_property(env, exports, mod_name_c_str.as_ptr(), exports_js_mod),
                "Set exports Object [{}] into exports object failed",
                js_mod_str
              );
              exports_objects.insert(js_mod_str.to_string());
            }
          }
          let (ctor, props): (Vec<_>, Vec<_>) = props.iter().partition(|prop| prop.is_ctor);

          let ctor = ctor.get(0).map(|c| c.raw().method.unwrap()).unwrap_or(noop);
          let raw_props: Vec<_> = props.iter().map(|prop| prop.raw()).collect();

          let js_class_name = CStr::from_bytes_with_nul_unchecked(js_name.as_bytes());
          let mut class_ptr = ptr::null_mut();

          check_status_or_throw!(
            env,
            sys::napi_define_class(
              env,
              js_class_name.as_ptr(),
              js_name.len() - 1,
              Some(ctor),
              ptr::null_mut(),
              raw_props.len(),
              raw_props.as_ptr(),
              &mut class_ptr,
            ),
            "Failed to register class `{}` generate by struct `{}`",
            &js_name,
            &rust_name
          );

          let mut ctor_ref = ptr::null_mut();
          sys::napi_create_reference(env, class_ptr, 1, &mut ctor_ref);

          registered_classes.insert(js_name.to_string(), ctor_ref);

          check_status_or_throw!(
            env,
            sys::napi_set_named_property(
              env,
              if exports_js_mod.is_null() {
                exports
              } else {
                exports_js_mod
              },
              js_class_name.as_ptr(),
              class_ptr
            ),
            "Failed to register class `{}` generate by struct `{}`",
            &js_name,
            &rust_name
          );
        }
      }
    });
    registered_classes_ptr.store(
      Box::into_raw(Box::new(registered_classes)),
      Ordering::Relaxed,
    );
  });

  #[cfg(feature = "compat-mode")]
  MODULE_EXPORTS.borrow_mut(|inner| {
    inner.iter().for_each(|callback| unsafe {
      if let Err(e) = callback(env, exports) {
        JsError::from(e).throw_into(env);
      }
    })
  });

  REGISTERED.store(true, Ordering::SeqCst);
  exports
}

pub(crate) unsafe extern "C" fn noop(
  env: sys::napi_env,
  _info: sys::napi_callback_info,
) -> sys::napi_value {
  let inner = crate::bindgen_runtime::___CALL_FROM_FACTORY.get_or_default();
  if !inner.load(Ordering::Relaxed) {
    unsafe {
      sys::napi_throw_error(
        env,
        ptr::null_mut(),
        CStr::from_bytes_with_nul_unchecked(b"Class contains no `constructor`, can not new it!\0")
          .as_ptr(),
      );
    }
  }
  ptr::null_mut()
}
