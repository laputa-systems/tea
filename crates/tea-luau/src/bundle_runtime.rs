//! Luau execution for a closed [`Bundle`](super::bundle::Bundle).
//!
//! The host installs this runtime before calling `Lua::sandbox(true)`. The
//! runtime adds exactly one global, `require`, whose only authority is to load
//! relative source from the supplied bundle. It has no filesystem loader,
//! package path, network access, or virtual-module fallback.

use super::bundle::{Bundle, ModulePath, ResolveError};
use mlua::{Lua, RegistryKey, Value};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex};

type RuntimeStateHandle = Arc<Mutex<RuntimeState>>;

/// A per-VM closed Luau bundle runtime.
///
/// A runtime may be installed into one or more VMs. Each call to
/// [`install`](Self::install) creates a fresh VM-owned cache through Luau app
/// data; module values are never shared across VMs.
pub struct BundleRuntime {
    bundle: Arc<Bundle>,
}

struct RuntimeState {
    bundle: Arc<Bundle>,
    cache: BTreeMap<ModulePath, RegistryKey>,
    loading: Vec<ModulePath>,
}

impl BundleRuntime {
    /// Create a runtime for a validated bundle.
    pub fn new(bundle: Bundle) -> Self {
        Self {
            bundle: Arc::new(bundle),
        }
    }

    /// Return the bundle this runtime will load.
    pub fn bundle(&self) -> &Bundle {
        &self.bundle
    }

    /// Install the closed relative `require` function into a Lua VM.
    ///
    /// Call this before `Lua::sandbox(true)`. The VM's app data stores the
    /// cache and loading stack, ensuring that module values belong to this VM.
    /// Installation replaces Luau's built-in `require` loader and refuses to
    /// replace another installed bundle runtime.
    pub fn install(&self, lua: &Lua) -> Result<(), BundleRuntimeError> {
        if lua.app_data_ref::<RuntimeStateHandle>().is_some() {
            return Err(BundleRuntimeError::AlreadyInstalled);
        }
        let state = Arc::new(Mutex::new(RuntimeState {
            bundle: Arc::clone(&self.bundle),
            cache: BTreeMap::new(),
            loading: Vec::new(),
        }));
        let callback_state = Arc::clone(&state);
        let require = lua
            .create_function(move |lua, request: String| {
                load_required(lua, &callback_state, &request).map_err(mlua::Error::external)
            })
            .map_err(|error| BundleRuntimeError::Install {
                message: error.to_string(),
            })?;

        // No user-visible operation can interleave between the app-data check
        // above and this insertion on one Lua state. `try_set_app_data` still
        // avoids a panic if a callback happens to hold the app-data borrow.
        lua.try_set_app_data(state)
            .map_err(|_| BundleRuntimeError::Install {
                message: "Lua application data is currently borrowed".to_owned(),
            })?;

        // Install the global last. If sandbox mode or another host policy
        // rejects the write, remove the state so a failed installation cannot
        // leave a half-installed runtime behind.
        if let Err(error) = lua.globals().set("require", require) {
            lua.remove_app_data::<RuntimeStateHandle>();
            return Err(BundleRuntimeError::Install {
                message: error.to_string(),
            });
        }
        Ok(())
    }

    /// Evaluate and return the bundle entrypoint's value.
    ///
    /// The entrypoint is evaluated once per VM. Subsequent calls return the
    /// cached value from that VM's registry. `install` must have succeeded on
    /// the same Lua state first.
    pub fn eval_entrypoint(&self, lua: &Lua) -> Result<Value, BundleRuntimeError> {
        let state = installed_state(lua)?;
        {
            let guard = lock_state(&state)?;
            if !Arc::ptr_eq(&guard.bundle, &self.bundle) {
                return Err(BundleRuntimeError::BundleMismatch);
            }
        }
        let entrypoint = self.bundle.manifest().entrypoint().clone();
        load_module(lua, &state, entrypoint)
    }

    /// Evaluate and return one named module's value.
    ///
    /// The module resolves its own relative imports inside this bundle, so a
    /// checked-in extension can factor shared code out of an executable
    /// handler without gaining any authority beyond the closed bundle.
    /// `install` must have succeeded on the same Lua state first.
    pub fn eval_module(&self, lua: &Lua, path: &str) -> Result<Value, BundleRuntimeError> {
        let state = installed_state(lua)?;
        {
            let guard = lock_state(&state)?;
            if !Arc::ptr_eq(&guard.bundle, &self.bundle) {
                return Err(BundleRuntimeError::BundleMismatch);
            }
        }
        let path = ModulePath::new(path)
            .map_err(|error| BundleRuntimeError::Resolve(ResolveError::InvalidPath(error)))?;
        load_module(lua, &state, path)
    }
}

fn installed_state(lua: &Lua) -> Result<RuntimeStateHandle, BundleRuntimeError> {
    lua.app_data_ref::<RuntimeStateHandle>()
        .map(|state| Arc::clone(&*state))
        .ok_or(BundleRuntimeError::NotInstalled)
}

fn load_required(
    lua: &Lua,
    state: &RuntimeStateHandle,
    request: &str,
) -> Result<Value, BundleRuntimeError> {
    if request.starts_with('@') {
        return Err(BundleRuntimeError::VirtualModuleDenied {
            request: request.to_owned(),
        });
    }
    if request.starts_with('/')
        || request.starts_with('\\')
        || request.as_bytes().get(1) == Some(&b':')
    {
        return Err(BundleRuntimeError::AbsoluteImport {
            request: request.to_owned(),
        });
    }
    if !is_relative_request(request) {
        return Err(BundleRuntimeError::NonRelativeImport {
            request: request.to_owned(),
        });
    }

    let (requester, bundle) = {
        let guard = lock_state(state)?;
        let requester =
            guard
                .loading
                .last()
                .cloned()
                .ok_or(BundleRuntimeError::NoRequesterContext {
                    request: request.to_owned(),
                })?;
        (requester, Arc::clone(&guard.bundle))
    };
    let path = bundle
        .resolve_relative(&requester, request)
        .map_err(BundleRuntimeError::Resolve)
        .map(|module| module.path)?;
    load_module(lua, state, path)
}

fn is_relative_request(request: &str) -> bool {
    request == "." || request == ".." || request.starts_with("./") || request.starts_with("../")
}

fn load_module(
    lua: &Lua,
    state: &RuntimeStateHandle,
    path: ModulePath,
) -> Result<Value, BundleRuntimeError> {
    let source = {
        let mut guard = lock_state(state)?;
        if let Some(key) = guard.cache.get(&path) {
            return lua
                .registry_value(key)
                .map_err(|error| BundleRuntimeError::Registry {
                    message: error.to_string(),
                });
        }
        if guard.loading.iter().any(|loading| loading == &path) {
            return Err(BundleRuntimeError::Cycle { path });
        }
        let source = guard
            .bundle
            .module(&path)
            .ok_or_else(|| BundleRuntimeError::ModuleNotFound { path: path.clone() })?
            .to_owned();
        guard.loading.push(path.clone());
        source
    };

    let evaluation = lua
        .load(source)
        .set_name(format!("=bundle:{}", path.as_str()))
        .eval::<Value>();

    match evaluation {
        Ok(value) => {
            let key = lua.create_registry_value(value.clone()).map_err(|error| {
                BundleRuntimeError::Registry {
                    message: error.to_string(),
                }
            });
            let mut guard = lock_state(state)?;
            finish_loading(&mut guard, &path);
            let key = key?;
            guard.cache.insert(path, key);
            Ok(value)
        }
        Err(error) => {
            let mut guard = lock_state(state)?;
            finish_loading(&mut guard, &path);
            if let Some(bundle_error) = error.downcast_ref::<BundleRuntimeError>() {
                Err(bundle_error.clone())
            } else {
                Err(BundleRuntimeError::Lua {
                    module: path,
                    message: error.to_string(),
                })
            }
        }
    }
}

fn finish_loading(state: &mut RuntimeState, path: &ModulePath) {
    if state.loading.last() == Some(path) {
        state.loading.pop();
    } else if let Some(index) = state.loading.iter().rposition(|loading| loading == path) {
        state.loading.remove(index);
    }
}

fn lock_state(
    state: &RuntimeStateHandle,
) -> Result<std::sync::MutexGuard<'_, RuntimeState>, BundleRuntimeError> {
    state.lock().map_err(|_| BundleRuntimeError::StatePoisoned)
}

/// A typed failure from bundle installation, loading, or `require` resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BundleRuntimeError {
    /// The VM already contains this runtime's app-data slot.
    AlreadyInstalled,
    /// Runtime installation failed inside `mlua`.
    Install {
        /// Host-safe diagnostic from `mlua`.
        message: String,
    },
    /// The runtime has not been installed into this VM.
    NotInstalled,
    /// The runtime was installed for a different bundle.
    BundleMismatch,
    /// The runtime state mutex was poisoned.
    StatePoisoned,
    /// A virtual capability module such as `@world` is not provided here.
    VirtualModuleDenied {
        /// Rejected virtual module request.
        request: String,
    },
    /// An absolute or host-style drive path was requested.
    AbsoluteImport {
        /// Rejected absolute or drive-prefixed module request.
        request: String,
    },
    /// Only `./` and `../` bundle imports are accepted.
    NonRelativeImport {
        /// Rejected bare package-style module request.
        request: String,
    },
    /// `require` was called outside a module evaluation.
    NoRequesterContext {
        /// Request made when no bundle module was evaluating.
        request: String,
    },
    /// The relative resolver rejected the request.
    Resolve(ResolveError),
    /// The requested module is absent from the bundle.
    ModuleNotFound {
        /// Missing canonical bundle-local path.
        path: ModulePath,
    },
    /// A module directly or indirectly required itself.
    Cycle {
        /// Module path already on this VM's active import stack.
        path: ModulePath,
    },
    /// The module's Luau source failed to evaluate.
    Lua {
        /// Canonical source module that failed to evaluate.
        module: ModulePath,
        /// Host-safe Luau diagnostic.
        message: String,
    },
    /// A cached module value could not be read or stored in the VM registry.
    Registry {
        /// Host-safe registry storage diagnostic.
        message: String,
    },
}

impl fmt::Display for BundleRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInstalled => formatter.write_str("bundle runtime is already installed"),
            Self::Install { message } => {
                write!(formatter, "bundle runtime installation failed: {message}")
            }
            Self::NotInstalled => {
                formatter.write_str("bundle runtime is not installed in this Lua VM")
            }
            Self::BundleMismatch => {
                formatter.write_str("Lua VM contains a different bundle runtime")
            }
            Self::StatePoisoned => formatter.write_str("bundle runtime state mutex was poisoned"),
            Self::VirtualModuleDenied { request } => {
                write!(formatter, "virtual module is not available: {request:?}")
            }
            Self::AbsoluteImport { request } => {
                write!(formatter, "absolute module import denied: {request:?}")
            }
            Self::NonRelativeImport { request } => {
                write!(formatter, "non-relative module import denied: {request:?}")
            }
            Self::NoRequesterContext { request } => {
                write!(
                    formatter,
                    "module request has no active requester: {request:?}"
                )
            }
            Self::Resolve(error) => write!(formatter, "module resolution failed: {error}"),
            Self::ModuleNotFound { path } => {
                write!(formatter, "bundle module is not present: {path}")
            }
            Self::Cycle { path } => write!(formatter, "cyclic bundle import at {path}"),
            Self::Lua { module, message } => {
                write!(formatter, "bundle module {module} failed: {message}")
            }
            Self::Registry { message } => {
                write!(formatter, "bundle module registry failed: {message}")
            }
        }
    }
}

impl Error for BundleRuntimeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{BundleManifest, BUNDLE_ABI_VERSION};
    use mlua::{Lua, StdLib};

    fn bundle(sources: impl IntoIterator<Item = (&'static str, &'static str)>) -> Bundle {
        Bundle::from_sources(
            BundleManifest::new(BUNDLE_ABI_VERSION, "main.luau", std::iter::empty::<&str>())
                .unwrap(),
            sources,
        )
        .unwrap()
    }

    fn owned_bundle(sources: impl IntoIterator<Item = (String, String)>) -> Bundle {
        Bundle::from_sources(
            BundleManifest::new(BUNDLE_ABI_VERSION, "main.luau", std::iter::empty::<&str>())
                .unwrap(),
            sources,
        )
        .unwrap()
    }

    fn lua() -> Lua {
        Lua::new_with(
            StdLib::COROUTINE | StdLib::TABLE | StdLib::STRING,
            mlua::LuaOptions::new(),
        )
        .unwrap()
    }

    #[test]
    fn relative_require_resolves_and_caches_per_vm() {
        let runtime = BundleRuntime::new(bundle([
            ("main.luau", "local x = require('./value.luau'); return x"),
            ("value.luau", "return { answer = 42 }"),
        ]));
        let first_lua = lua();
        runtime.install(&first_lua).unwrap();
        first_lua.sandbox(true).unwrap();
        let first = runtime.eval_entrypoint(&first_lua).unwrap();
        assert_eq!(first.as_table().unwrap().get::<i32>("answer").unwrap(), 42);
        let second = runtime.eval_entrypoint(&first_lua).unwrap();
        assert!(first == second);

        let second_lua = lua();
        runtime.install(&second_lua).unwrap();
        let second_vm_value = runtime.eval_entrypoint(&second_lua).unwrap();
        assert_eq!(
            second_vm_value
                .as_table()
                .unwrap()
                .get::<i32>("answer")
                .unwrap(),
            42
        );
    }

    #[test]
    fn virtual_absolute_and_non_relative_imports_are_denied() {
        let cases = [
            (
                "@world",
                BundleRuntimeError::VirtualModuleDenied {
                    request: "@world".to_owned(),
                },
            ),
            (
                "/tmp/module.luau",
                BundleRuntimeError::AbsoluteImport {
                    request: "/tmp/module.luau".to_owned(),
                },
            ),
            (
                "module.luau",
                BundleRuntimeError::NonRelativeImport {
                    request: "module.luau".to_owned(),
                },
            ),
        ];
        for (request, expected) in cases {
            let runtime = BundleRuntime::new(owned_bundle([(
                "main.luau".to_owned(),
                format!("return require({request:?})"),
            )]));
            let lua = lua();
            runtime.install(&lua).unwrap();
            let error = runtime.eval_entrypoint(&lua).unwrap_err();
            assert_eq!(error, expected);
        }
    }

    #[test]
    fn cycles_and_unknown_relative_modules_are_typed() {
        let cycle_runtime = BundleRuntime::new(bundle([
            ("main.luau", "return require('./a.luau')"),
            ("a.luau", "return require('./main.luau')"),
        ]));
        let cycle_lua = lua();
        cycle_runtime.install(&cycle_lua).unwrap();
        assert_eq!(
            cycle_runtime.eval_entrypoint(&cycle_lua).unwrap_err(),
            BundleRuntimeError::Cycle {
                path: ModulePath::new("main.luau").unwrap()
            }
        );

        let missing_runtime =
            BundleRuntime::new(bundle([("main.luau", "return require('./missing.luau')")]));
        let missing_lua = lua();
        missing_runtime.install(&missing_lua).unwrap();
        assert_eq!(
            missing_runtime.eval_entrypoint(&missing_lua).unwrap_err(),
            BundleRuntimeError::Resolve(ResolveError::NotFound {
                path: ModulePath::new("missing.luau").unwrap()
            })
        );
    }

    #[test]
    fn installation_replaces_builtin_require_and_rejects_duplicate_runtime() {
        let runtime = BundleRuntime::new(bundle([("main.luau", "return 1")]));
        let lua = lua();
        runtime.install(&lua).unwrap();
        assert_eq!(
            runtime.install(&lua).unwrap_err(),
            BundleRuntimeError::AlreadyInstalled
        );
    }
}
