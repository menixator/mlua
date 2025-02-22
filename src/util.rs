use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt::Write;
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::{mem, ptr, slice};

use once_cell::sync::Lazy;

use crate::error::{Error, Result};
use crate::ffi;

static METATABLE_CACHE: Lazy<Mutex<HashMap<TypeId, u8>>> = Lazy::new(|| {
    // The capacity must(!) be greater than number of stored keys
    Mutex::new(HashMap::with_capacity(32))
});

// Checks that Lua has enough free stack space for future stack operations. On failure, this will
// panic with an internal error message.
pub unsafe fn assert_stack(state: *mut ffi::lua_State, amount: c_int) {
    // TODO: This should only be triggered when there is a logic error in `mlua`. In the future,
    // when there is a way to be confident about stack safety and test it, this could be enabled
    // only when `cfg!(debug_assertions)` is true.
    mlua_assert!(
        ffi::lua_checkstack(state, amount) != 0,
        "out of stack space"
    );
}

// Checks that Lua has enough free stack space and returns `Error::StackError` on failure.
pub unsafe fn check_stack(state: *mut ffi::lua_State, amount: c_int) -> Result<()> {
    if ffi::lua_checkstack(state, amount) == 0 {
        Err(Error::StackError)
    } else {
        Ok(())
    }
}

pub struct StackGuard {
    state: *mut ffi::lua_State,
    top: c_int,
    extra: c_int,
}

impl StackGuard {
    // Creates a StackGuard instance with record of the stack size, and on Drop will check the
    // stack size and drop any extra elements. If the stack size at the end is *smaller* than at
    // the beginning, this is considered a fatal logic error and will result in a panic.
    pub unsafe fn new(state: *mut ffi::lua_State) -> StackGuard {
        StackGuard {
            state,
            top: ffi::lua_gettop(state),
            extra: 0,
        }
    }

    // Similar to `new`, but checks and keeps `extra` elements from top of the stack on Drop.
    pub unsafe fn new_extra(state: *mut ffi::lua_State, extra: c_int) -> StackGuard {
        StackGuard {
            state,
            top: ffi::lua_gettop(state),
            extra,
        }
    }
}

impl Drop for StackGuard {
    fn drop(&mut self) {
        unsafe {
            let top = ffi::lua_gettop(self.state);
            if top < self.top + self.extra {
                mlua_panic!("{} too many stack values popped", self.top - top)
            }
            if top > self.top + self.extra {
                if self.extra > 0 {
                    ffi::lua_rotate(self.state, self.top + 1, self.extra);
                }
                ffi::lua_settop(self.state, self.top + self.extra);
            }
        }
    }
}

// Call a function that calls into the Lua API and may trigger a Lua error (longjmp) in a safe way.
// Wraps the inner function in a call to `lua_pcall`, so the inner function only has access to a
// limited lua stack. `nargs` and `nresults` are similar to the parameters of `lua_pcall`, but the
// given function return type is not the return value count, instead the inner function return
// values are assumed to match the `nresults` param. Provided function must *not* panic, and since it
// will generally be lonjmping, should not contain any values that implements Drop.
// Internally uses 3 extra stack spaces, and does not call checkstack.
pub unsafe fn protect_lua<F, R>(
    state: *mut ffi::lua_State,
    nargs: c_int,
    nresults: c_int,
    f: F,
) -> Result<R>
where
    F: Fn(*mut ffi::lua_State) -> R,
    R: Copy,
{
    union URes<R: Copy> {
        uninit: (),
        init: R,
    }

    struct Params<F, R: Copy> {
        function: F,
        result: URes<R>,
        nresults: c_int,
    }

    unsafe extern "C" fn do_call<F, R>(state: *mut ffi::lua_State) -> c_int
    where
        R: Copy,
        F: Fn(*mut ffi::lua_State) -> R,
    {
        let params = ffi::lua_touserdata(state, -1) as *mut Params<F, R>;
        ffi::lua_pop(state, 1);

        (*params).result.init = ((*params).function)(state);

        if (*params).nresults == ffi::LUA_MULTRET {
            ffi::lua_gettop(state)
        } else {
            (*params).nresults
        }
    }

    let stack_start = ffi::lua_gettop(state) - nargs;

    ffi::lua_pushcfunction(state, error_traceback);
    ffi::lua_pushcfunction(state, do_call::<F, R>);
    if nargs > 0 {
        ffi::lua_rotate(state, stack_start + 1, 2);
    }

    let mut params = Params {
        function: f,
        result: URes { uninit: () },
        nresults,
    };

    ffi::lua_pushlightuserdata(state, &mut params as *mut Params<F, R> as *mut c_void);
    let ret = ffi::lua_pcall(state, nargs + 1, nresults, stack_start + 1);
    ffi::lua_remove(state, stack_start + 1);

    if ret == ffi::LUA_OK {
        // `LUA_OK` is only returned when the `do_call` function has completed successfully, so
        // `params.result` is definitely initialized.
        Ok(params.result.init)
    } else {
        Err(pop_error(state, ret))
    }
}

// Pops an error off of the stack and returns it. The specific behavior depends on the type of the
// error at the top of the stack:
//   1) If the error is actually a WrappedPanic, this will continue the panic.
//   2) If the error on the top of the stack is actually a WrappedError, just returns it.
//   3) Otherwise, interprets the error as the appropriate lua error.
// Uses 2 stack spaces, does not call checkstack.
pub unsafe fn pop_error(state: *mut ffi::lua_State, err_code: c_int) -> Error {
    mlua_debug_assert!(
        err_code != ffi::LUA_OK && err_code != ffi::LUA_YIELD,
        "pop_error called with non-error return code"
    );

    match get_gc_userdata::<WrappedFailure>(state, -1).as_mut() {
        Some(WrappedFailure::Error(err)) => {
            ffi::lua_pop(state, 1);
            err.clone()
        }
        Some(WrappedFailure::Panic(panic)) => {
            if let Some(p) = panic.take() {
                resume_unwind(p);
            } else {
                Error::PreviouslyResumedPanic
            }
        }
        _ => {
            let err_string = to_string(state, -1);
            ffi::lua_pop(state, 1);

            match err_code {
                ffi::LUA_ERRRUN => Error::RuntimeError(err_string),
                ffi::LUA_ERRSYNTAX => {
                    Error::SyntaxError {
                        // This seems terrible, but as far as I can tell, this is exactly what the
                        // stock Lua REPL does.
                        incomplete_input: err_string.ends_with("<eof>")
                            || err_string.ends_with("'<eof>'"),
                        message: err_string,
                    }
                }
                ffi::LUA_ERRERR => {
                    // This error is raised when the error handler raises an error too many times
                    // recursively, and continuing to trigger the error handler would cause a stack
                    // overflow. It is not very useful to differentiate between this and "ordinary"
                    // runtime errors, so we handle them the same way.
                    Error::RuntimeError(err_string)
                }
                ffi::LUA_ERRMEM => Error::MemoryError(err_string),
                #[cfg(any(feature = "lua53", feature = "lua52"))]
                ffi::LUA_ERRGCMM => Error::GarbageCollectorError(err_string),
                _ => mlua_panic!("unrecognized lua error code"),
            }
        }
    }
}

// Uses 3 stack spaces
pub unsafe fn push_string<S: AsRef<[u8]> + ?Sized>(
    state: *mut ffi::lua_State,
    s: &S,
) -> Result<()> {
    let s = s.as_ref();
    protect_lua(state, 0, 1, |state| {
        ffi::lua_pushlstring(state, s.as_ptr() as *const c_char, s.len());
    })
}

// Uses 3 stack spaces
pub unsafe fn push_table(state: *mut ffi::lua_State, narr: c_int, nrec: c_int) -> Result<()> {
    protect_lua(state, 0, 1, |state| ffi::lua_createtable(state, narr, nrec))
}

// Uses 4 stack spaces
pub unsafe fn rawset_field<S>(state: *mut ffi::lua_State, table: c_int, field: &S) -> Result<()>
where
    S: AsRef<[u8]> + ?Sized,
{
    let field = field.as_ref();
    ffi::lua_pushvalue(state, table);
    protect_lua(state, 2, 0, |state| {
        ffi::lua_pushlstring(state, field.as_ptr() as *const c_char, field.len());
        ffi::lua_rotate(state, -3, 2);
        ffi::lua_rawset(state, -3);
    })
}

// Internally uses 3 stack spaces, does not call checkstack.
pub unsafe fn push_userdata<T>(state: *mut ffi::lua_State, t: T) -> Result<()> {
    let ud = protect_lua(state, 0, 1, |state| {
        ffi::lua_newuserdata(state, mem::size_of::<T>()) as *mut T
    })?;
    ptr::write(ud, t);
    Ok(())
}

pub unsafe fn get_userdata<T>(state: *mut ffi::lua_State, index: c_int) -> *mut T {
    let ud = ffi::lua_touserdata(state, index) as *mut T;
    mlua_debug_assert!(!ud.is_null(), "userdata pointer is null");
    ud
}

// Pops the userdata off of the top of the stack and returns it to rust, invalidating the lua
// userdata and gives it the special "destructed" userdata metatable. Userdata must not have been
// previously invalidated, and this method does not check for this.
// Uses 1 extra stack space and does not call checkstack.
pub unsafe fn take_userdata<T>(state: *mut ffi::lua_State) -> T {
    // We set the metatable of userdata on __gc to a special table with no __gc method and with
    // metamethods that trigger an error on access. We do this so that it will not be double
    // dropped, and also so that it cannot be used or identified as any particular userdata type
    // after the first call to __gc.
    get_destructed_userdata_metatable(state);
    ffi::lua_setmetatable(state, -2);
    let ud = get_userdata(state, -1);
    ffi::lua_pop(state, 1);
    ptr::read(ud)
}

// Pushes the userdata and attaches a metatable with __gc method.
// Internally uses 3 stack spaces, does not call checkstack.
pub unsafe fn push_gc_userdata<T: Any>(state: *mut ffi::lua_State, t: T) -> Result<()> {
    push_userdata(state, t)?;
    get_gc_metatable::<T>(state);
    ffi::lua_setmetatable(state, -2);
    Ok(())
}

// Uses 2 stack spaces, does not call checkstack
pub unsafe fn get_gc_userdata<T: Any>(state: *mut ffi::lua_State, index: c_int) -> *mut T {
    let ud = ffi::lua_touserdata(state, index) as *mut T;
    if ud.is_null() || ffi::lua_getmetatable(state, index) == 0 {
        return ptr::null_mut();
    }
    get_gc_metatable::<T>(state);
    let res = ffi::lua_rawequal(state, -1, -2);
    ffi::lua_pop(state, 2);
    if res == 0 {
        return ptr::null_mut();
    }
    ud
}

// Populates the given table with the appropriate members to be a userdata metatable for the given type.
// This function takes the given table at the `metatable` index, and adds an appropriate `__gc` member
// to it for the given type and a `__metatable` entry to protect the table from script access.
// The function also, if given a `field_getters` or `methods` tables, will create an `__index` metamethod
// (capturing previous one) to lookup in `field_getters` first, then `methods` and falling back to the
// captured `__index` if no matches found.
// The same is also applicable for `__newindex` metamethod and `field_setters` table.
// Internally uses 9 stack spaces and does not call checkstack.
pub unsafe fn init_userdata_metatable<T>(
    state: *mut ffi::lua_State,
    metatable: c_int,
    field_getters: Option<c_int>,
    field_setters: Option<c_int>,
    methods: Option<c_int>,
) -> Result<()> {
    // Wrapper to lookup in `field_getters` first, then `methods`, ending original `__index`.
    // Used only if `field_getters` or `methods` set.
    unsafe extern "C" fn meta_index_impl(state: *mut ffi::lua_State) -> c_int {
        // stack: self, key
        ffi::luaL_checkstack(state, 2, ptr::null());

        // lookup in `field_getters` table
        if ffi::lua_isnil(state, ffi::lua_upvalueindex(2)) == 0 {
            ffi::lua_pushvalue(state, -1); // `key` arg
            if ffi::lua_rawget(state, ffi::lua_upvalueindex(2)) != ffi::LUA_TNIL {
                ffi::lua_insert(state, -3); // move function
                ffi::lua_pop(state, 1); // remove `key`
                ffi::lua_call(state, 1, 1);
                return 1;
            }
            ffi::lua_pop(state, 1); // pop the nil value
        }
        // lookup in `methods` table
        if ffi::lua_isnil(state, ffi::lua_upvalueindex(3)) == 0 {
            ffi::lua_pushvalue(state, -1); // `key` arg
            if ffi::lua_rawget(state, ffi::lua_upvalueindex(3)) != ffi::LUA_TNIL {
                ffi::lua_insert(state, -3);
                ffi::lua_pop(state, 2);
                return 1;
            }
            ffi::lua_pop(state, 1); // pop the nil value
        }

        // lookup in `__index`
        ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
        match ffi::lua_type(state, -1) {
            ffi::LUA_TNIL => {
                ffi::lua_pop(state, 1); // pop the nil value
                let field = ffi::lua_tostring(state, -1);
                ffi::luaL_error(state, cstr!("attempt to get an unknown field '%s'"), field);
            }
            ffi::LUA_TTABLE => {
                ffi::lua_insert(state, -2);
                ffi::lua_gettable(state, -2);
            }
            ffi::LUA_TFUNCTION => {
                ffi::lua_insert(state, -3);
                ffi::lua_call(state, 2, 1);
            }
            _ => unreachable!(),
        }

        1
    }

    // Similar to `meta_index_impl`, checks `field_setters` table first, then `__newindex` metamethod.
    // Used only if `field_setters` set.
    unsafe extern "C" fn meta_newindex_impl(state: *mut ffi::lua_State) -> c_int {
        // stack: self, key, value
        ffi::luaL_checkstack(state, 2, ptr::null());

        // lookup in `field_setters` table
        ffi::lua_pushvalue(state, -2); // `key` arg
        if ffi::lua_rawget(state, ffi::lua_upvalueindex(2)) != ffi::LUA_TNIL {
            ffi::lua_remove(state, -3); // remove `key`
            ffi::lua_insert(state, -3); // move function
            ffi::lua_call(state, 2, 0);
            return 0;
        }
        ffi::lua_pop(state, 1); // pop the nil value

        // lookup in `__newindex`
        ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
        match ffi::lua_type(state, -1) {
            ffi::LUA_TNIL => {
                ffi::lua_pop(state, 1); // pop the nil value
                let field = ffi::lua_tostring(state, -2);
                ffi::luaL_error(state, cstr!("attempt to set an unknown field '%s'"), field);
            }
            ffi::LUA_TTABLE => {
                ffi::lua_insert(state, -3);
                ffi::lua_settable(state, -3);
            }
            ffi::LUA_TFUNCTION => {
                ffi::lua_insert(state, -4);
                ffi::lua_call(state, 3, 0);
            }
            _ => unreachable!(),
        }

        0
    }

    ffi::lua_pushvalue(state, metatable);

    if field_getters.is_some() || methods.is_some() {
        push_string(state, "__index")?;
        let index_type = ffi::lua_rawget(state, -2);
        match index_type {
            ffi::LUA_TNIL | ffi::LUA_TTABLE | ffi::LUA_TFUNCTION => {
                for &idx in &[field_getters, methods] {
                    if let Some(idx) = idx {
                        ffi::lua_pushvalue(state, idx);
                    } else {
                        ffi::lua_pushnil(state);
                    }
                }
                protect_lua(state, 3, 1, |state| {
                    ffi::lua_pushcclosure(state, meta_index_impl, 3);
                })?;
            }
            _ => mlua_panic!("improper __index type {}", index_type),
        }

        rawset_field(state, -2, "__index")?;
    }

    if let Some(field_setters) = field_setters {
        push_string(state, "__newindex")?;
        let newindex_type = ffi::lua_rawget(state, -2);
        match newindex_type {
            ffi::LUA_TNIL | ffi::LUA_TTABLE | ffi::LUA_TFUNCTION => {
                ffi::lua_pushvalue(state, field_setters);
                protect_lua(state, 2, 1, |state| {
                    ffi::lua_pushcclosure(state, meta_newindex_impl, 2);
                })?;
            }
            _ => mlua_panic!("improper __newindex type {}", newindex_type),
        }

        rawset_field(state, -2, "__newindex")?;
    }

    ffi::lua_pushcfunction(state, userdata_destructor::<T>);
    rawset_field(state, -2, "__gc")?;

    ffi::lua_pushboolean(state, 0);
    rawset_field(state, -2, "__metatable")?;

    ffi::lua_pop(state, 1);

    Ok(())
}

pub unsafe extern "C" fn userdata_destructor<T>(state: *mut ffi::lua_State) -> c_int {
    // It's probably NOT a good idea to catch Rust panics in finalizer
    // Lua 5.4 ignores it, other versions generates `LUA_ERRGCMM` without calling message handler
    take_userdata::<T>(state);
    0
}

// In the context of a lua callback, this will call the given function and if the given function
// returns an error, *or if the given function panics*, this will result in a call to `lua_error` (a
// longjmp). The error or panic is wrapped in such a way that when calling `pop_error` back on
// the Rust side, it will resume the panic.
//
// This function assumes the structure of the stack at the beginning of a callback, that the only
// elements on the stack are the arguments to the callback.
//
// This function uses some of the bottom of the stack for error handling, the given callback will be
// given the number of arguments available as an argument, and should return the number of returns
// as normal, but cannot assume that the arguments available start at 0.
pub unsafe fn callback_error<F, R>(state: *mut ffi::lua_State, f: F) -> R
where
    F: FnOnce(c_int) -> Result<R>,
{
    let nargs = ffi::lua_gettop(state);

    // We need 2 extra stack spaces to store preallocated memory and error/panic metatable
    let extra_stack = if nargs < 2 { 2 - nargs } else { 1 };
    ffi::luaL_checkstack(
        state,
        extra_stack,
        cstr!("not enough stack space for callback error handling"),
    );

    // We cannot shadow Rust errors with Lua ones, we pre-allocate enough memory
    // to store a wrapped error or panic *before* we proceed.
    let ud = ffi::lua_newuserdata(state, mem::size_of::<WrappedFailure>());
    ffi::lua_rotate(state, 1, 1);

    match catch_unwind(AssertUnwindSafe(|| f(nargs))) {
        Ok(Ok(r)) => {
            ffi::lua_remove(state, 1);
            r
        }
        Ok(Err(err)) => {
            ffi::lua_settop(state, 1);

            let wrapped_error = ud as *mut WrappedFailure;
            ptr::write(wrapped_error, WrappedFailure::Error(err));
            get_gc_metatable::<WrappedFailure>(state);
            ffi::lua_setmetatable(state, -2);

            // Convert to CallbackError and attach traceback
            let traceback = if ffi::lua_checkstack(state, ffi::LUA_TRACEBACK_STACK) != 0 {
                ffi::luaL_traceback(state, state, ptr::null(), 0);
                let traceback = to_string(state, -1);
                ffi::lua_pop(state, 1);
                traceback
            } else {
                "<not enough stack space for traceback>".to_string()
            };
            if let WrappedFailure::Error(ref mut err) = *wrapped_error {
                let cause = Arc::new(err.clone());
                *err = Error::CallbackError { traceback, cause };
            }

            ffi::lua_error(state)
        }
        Err(p) => {
            ffi::lua_settop(state, 1);
            ptr::write(ud as *mut WrappedFailure, WrappedFailure::Panic(Some(p)));
            get_gc_metatable::<WrappedFailure>(state);
            ffi::lua_setmetatable(state, -2);
            ffi::lua_error(state)
        }
    }
}

pub unsafe extern "C" fn error_traceback(state: *mut ffi::lua_State) -> c_int {
    if ffi::lua_checkstack(state, 2) == 0 {
        // If we don't have enough stack space to even check the error type, do
        // nothing so we don't risk shadowing a rust panic.
        return 1;
    }

    if get_gc_userdata::<WrappedFailure>(state, -1).is_null() {
        let s = ffi::luaL_tolstring(state, -1, ptr::null_mut());
        if ffi::lua_checkstack(state, ffi::LUA_TRACEBACK_STACK) != 0 {
            ffi::luaL_traceback(state, state, s, 1);
            ffi::lua_remove(state, -2);
        }
    }

    1
}

// A variant of `pcall` that does not allow Lua to catch Rust panics from `callback_error`.
pub unsafe extern "C" fn safe_pcall(state: *mut ffi::lua_State) -> c_int {
    ffi::luaL_checkstack(state, 2, ptr::null());

    let top = ffi::lua_gettop(state);
    if top == 0 {
        ffi::lua_pushstring(state, cstr!("not enough arguments to pcall"));
        ffi::lua_error(state);
    }

    if ffi::lua_pcall(state, top - 1, ffi::LUA_MULTRET, 0) == ffi::LUA_OK {
        ffi::lua_pushboolean(state, 1);
        ffi::lua_insert(state, 1);
        ffi::lua_gettop(state)
    } else {
        if let Some(WrappedFailure::Panic(_)) =
            get_gc_userdata::<WrappedFailure>(state, -1).as_ref()
        {
            ffi::lua_error(state);
        }
        ffi::lua_pushboolean(state, 0);
        ffi::lua_insert(state, -2);
        2
    }
}

// A variant of `xpcall` that does not allow Lua to catch Rust panics from `callback_error`.
pub unsafe extern "C" fn safe_xpcall(state: *mut ffi::lua_State) -> c_int {
    unsafe extern "C" fn xpcall_msgh(state: *mut ffi::lua_State) -> c_int {
        ffi::luaL_checkstack(state, 2, ptr::null());

        if let Some(WrappedFailure::Panic(_)) =
            get_gc_userdata::<WrappedFailure>(state, -1).as_ref()
        {
            1
        } else {
            ffi::lua_pushvalue(state, ffi::lua_upvalueindex(1));
            ffi::lua_insert(state, 1);
            ffi::lua_call(state, ffi::lua_gettop(state) - 1, ffi::LUA_MULTRET);
            ffi::lua_gettop(state)
        }
    }

    ffi::luaL_checkstack(state, 2, ptr::null());

    let top = ffi::lua_gettop(state);
    if top < 2 {
        ffi::lua_pushstring(state, cstr!("not enough arguments to xpcall"));
        ffi::lua_error(state);
    }

    ffi::lua_pushvalue(state, 2);
    ffi::lua_pushcclosure(state, xpcall_msgh, 1);
    ffi::lua_copy(state, 1, 2);
    ffi::lua_replace(state, 1);

    if ffi::lua_pcall(state, ffi::lua_gettop(state) - 2, ffi::LUA_MULTRET, 1) == ffi::LUA_OK {
        ffi::lua_pushboolean(state, 1);
        ffi::lua_insert(state, 2);
        ffi::lua_gettop(state) - 1
    } else {
        if let Some(WrappedFailure::Panic(_)) =
            get_gc_userdata::<WrappedFailure>(state, -1).as_ref()
        {
            ffi::lua_error(state);
        }
        ffi::lua_pushboolean(state, 0);
        ffi::lua_insert(state, -2);
        2
    }
}

// Returns Lua main thread for Lua >= 5.2 or checks that the passed thread is main for Lua 5.1.
// Does not call lua_checkstack, uses 1 stack space.
pub unsafe fn get_main_state(state: *mut ffi::lua_State) -> Option<*mut ffi::lua_State> {
    #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
    {
        ffi::lua_rawgeti(state, ffi::LUA_REGISTRYINDEX, ffi::LUA_RIDX_MAINTHREAD);
        let main_state = ffi::lua_tothread(state, -1);
        ffi::lua_pop(state, 1);
        Some(main_state)
    }
    #[cfg(any(feature = "lua51", feature = "luajit"))]
    {
        // Check the current state first
        let is_main_state = ffi::lua_pushthread(state) == 1;
        ffi::lua_pop(state, 1);
        if is_main_state {
            Some(state)
        } else {
            None
        }
    }
}

// Initialize the internal (with __gc method) metatable for a type T.
// Uses 6 stack spaces and calls checkstack.
pub unsafe fn init_gc_metatable<T: Any>(
    state: *mut ffi::lua_State,
    customize_fn: Option<fn(*mut ffi::lua_State) -> Result<()>>,
) -> Result<()> {
    check_stack(state, 6)?;

    let type_id = TypeId::of::<T>();
    let ref_addr = {
        let mut mt_cache = mlua_expect!(METATABLE_CACHE.lock(), "cannot lock metatable cache");
        mlua_assert!(
            mt_cache.capacity() - mt_cache.len() > 0,
            "out of metatable cache capacity"
        );
        mt_cache.insert(type_id, 0);
        &mt_cache[&type_id] as *const u8
    };

    push_table(state, 0, 3)?;

    ffi::lua_pushcfunction(state, userdata_destructor::<T>);
    rawset_field(state, -2, "__gc")?;

    ffi::lua_pushboolean(state, 0);
    rawset_field(state, -2, "__metatable")?;

    if let Some(f) = customize_fn {
        f(state)?;
    }

    protect_lua(state, 1, 0, |state| {
        ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, ref_addr as *mut c_void);
    })?;

    Ok(())
}

pub unsafe fn get_gc_metatable<T: Any>(state: *mut ffi::lua_State) {
    let type_id = TypeId::of::<T>();
    let ref_addr = {
        let mt_cache = mlua_expect!(METATABLE_CACHE.lock(), "cannot lock metatable cache");
        mlua_expect!(mt_cache.get(&type_id), "gc metatable does not exist") as *const u8
    };
    ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, ref_addr as *const c_void);
}

// Initialize the error, panic, and destructed userdata metatables.
pub unsafe fn init_error_registry(state: *mut ffi::lua_State) -> Result<()> {
    check_stack(state, 7)?;

    // Create error and panic metatables

    unsafe extern "C" fn error_tostring(state: *mut ffi::lua_State) -> c_int {
        callback_error(state, |_| {
            check_stack(state, 3)?;

            let err_buf = match get_gc_userdata::<WrappedFailure>(state, -1).as_ref() {
                Some(WrappedFailure::Error(error)) => {
                    let err_buf_key = &ERROR_PRINT_BUFFER_KEY as *const u8 as *const c_void;
                    ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, err_buf_key);
                    let err_buf = ffi::lua_touserdata(state, -1) as *mut String;
                    ffi::lua_pop(state, 2);

                    (*err_buf).clear();
                    // Depending on how the API is used and what error types scripts are given, it may
                    // be possible to make this consume arbitrary amounts of memory (for example, some
                    // kind of recursive error structure?)
                    let _ = write!(&mut (*err_buf), "{}", error);
                    // Find first two sources that caused the error
                    let mut source1 = error.source();
                    let mut source0 = source1.and_then(|s| s.source());
                    while let Some(source) = source0.and_then(|s| s.source()) {
                        source1 = source0;
                        source0 = Some(source);
                    }
                    match (source1, source0) {
                        (_, Some(error0))
                            if error0.to_string().contains("\nstack traceback:\n") =>
                        {
                            let _ = write!(&mut (*err_buf), "\ncaused by: {}", error0);
                        }
                        (Some(error1), Some(error0)) => {
                            let _ = write!(&mut (*err_buf), "\ncaused by: {}", error0);
                            let s = error1.to_string();
                            if let Some(traceback) = s.splitn(2, "\nstack traceback:\n").nth(1) {
                                let _ =
                                    write!(&mut (*err_buf), "\nstack traceback:\n{}", traceback);
                            }
                        }
                        (Some(error1), None) => {
                            let _ = write!(&mut (*err_buf), "\ncaused by: {}", error1);
                        }
                        _ => {}
                    }
                    Ok(err_buf)
                }
                Some(WrappedFailure::Panic(Some(ref panic))) => {
                    let err_buf_key = &ERROR_PRINT_BUFFER_KEY as *const u8 as *const c_void;
                    ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, err_buf_key);
                    let err_buf = ffi::lua_touserdata(state, -1) as *mut String;
                    (*err_buf).clear();
                    ffi::lua_pop(state, 2);

                    if let Some(msg) = panic.downcast_ref::<&str>() {
                        let _ = write!(&mut (*err_buf), "{}", msg);
                    } else if let Some(msg) = panic.downcast_ref::<String>() {
                        let _ = write!(&mut (*err_buf), "{}", msg);
                    } else {
                        let _ = write!(&mut (*err_buf), "<panic>");
                    };
                    Ok(err_buf)
                }
                Some(WrappedFailure::Panic(None)) => Err(Error::PreviouslyResumedPanic),
                _ => {
                    // I'm not sure whether this is possible to trigger without bugs in mlua?
                    Err(Error::UserDataTypeMismatch)
                }
            }?;

            push_string(state, &*err_buf)?;
            (*err_buf).clear();

            Ok(1)
        })
    }

    init_gc_metatable::<WrappedFailure>(
        state,
        Some(|state| {
            ffi::lua_pushcfunction(state, error_tostring);
            rawset_field(state, -2, "__tostring")
        }),
    )?;

    // Create destructed userdata metatable

    unsafe extern "C" fn destructed_error(state: *mut ffi::lua_State) -> c_int {
        callback_error(state, |_| Err(Error::CallbackDestructed))
    }

    push_table(state, 0, 26)?;
    ffi::lua_pushcfunction(state, destructed_error);
    for &method in &[
        "__add",
        "__sub",
        "__mul",
        "__div",
        "__mod",
        "__pow",
        "__unm",
        #[cfg(any(feature = "lua54", feature = "lua53"))]
        "__idiv",
        #[cfg(any(feature = "lua54", feature = "lua53"))]
        "__band",
        #[cfg(any(feature = "lua54", feature = "lua53"))]
        "__bor",
        #[cfg(any(feature = "lua54", feature = "lua53"))]
        "__bxor",
        #[cfg(any(feature = "lua54", feature = "lua53"))]
        "__bnot",
        #[cfg(any(feature = "lua54", feature = "lua53"))]
        "__shl",
        #[cfg(any(feature = "lua54", feature = "lua53"))]
        "__shr",
        "__concat",
        "__len",
        "__eq",
        "__lt",
        "__le",
        "__index",
        "__newindex",
        "__call",
        "__tostring",
        #[cfg(any(feature = "lua54", feature = "lua53", feature = "lua52"))]
        "__pairs",
        #[cfg(any(feature = "lua53", feature = "lua52"))]
        "__ipairs",
        #[cfg(feature = "lua54")]
        "__close",
    ] {
        ffi::lua_pushvalue(state, -1);
        rawset_field(state, -3, method)?;
    }
    ffi::lua_pop(state, 1);

    protect_lua(state, 1, 0, |state| {
        let destructed_mt_key = &DESTRUCTED_USERDATA_METATABLE as *const u8 as *const c_void;
        ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, destructed_mt_key);
    })?;

    // Create error print buffer
    init_gc_metatable::<String>(state, None)?;
    push_gc_userdata(state, String::new())?;
    protect_lua(state, 1, 0, |state| {
        let err_buf_key = &ERROR_PRINT_BUFFER_KEY as *const u8 as *const c_void;
        ffi::lua_rawsetp(state, ffi::LUA_REGISTRYINDEX, err_buf_key);
    })?;

    Ok(())
}

pub(crate) enum WrappedFailure {
    Error(Error),
    Panic(Option<Box<dyn Any + Send + 'static>>),
}

// Converts the given lua value to a string in a reasonable format without causing a Lua error or
// panicking.
pub(crate) unsafe fn to_string(state: *mut ffi::lua_State, index: c_int) -> String {
    match ffi::lua_type(state, index) {
        ffi::LUA_TNONE => "<none>".to_string(),
        ffi::LUA_TNIL => "<nil>".to_string(),
        ffi::LUA_TBOOLEAN => (ffi::lua_toboolean(state, index) != 1).to_string(),
        ffi::LUA_TLIGHTUSERDATA => {
            format!("<lightuserdata {:?}>", ffi::lua_topointer(state, index))
        }
        ffi::LUA_TNUMBER => {
            let mut isint = 0;
            let i = ffi::lua_tointegerx(state, -1, &mut isint);
            if isint == 0 {
                ffi::lua_tonumber(state, index).to_string()
            } else {
                i.to_string()
            }
        }
        ffi::LUA_TSTRING => {
            let mut size = 0;
            // This will not trigger a 'm' error, because the reference is guaranteed to be of
            // string type
            let data = ffi::lua_tolstring(state, index, &mut size);
            String::from_utf8_lossy(slice::from_raw_parts(data as *const u8, size)).into_owned()
        }
        ffi::LUA_TTABLE => format!("<table {:?}>", ffi::lua_topointer(state, index)),
        ffi::LUA_TFUNCTION => format!("<function {:?}>", ffi::lua_topointer(state, index)),
        ffi::LUA_TUSERDATA => format!("<userdata {:?}>", ffi::lua_topointer(state, index)),
        ffi::LUA_TTHREAD => format!("<thread {:?}>", ffi::lua_topointer(state, index)),
        _ => "<unknown>".to_string(),
    }
}

pub(crate) unsafe fn get_destructed_userdata_metatable(state: *mut ffi::lua_State) {
    let key = &DESTRUCTED_USERDATA_METATABLE as *const u8 as *const c_void;
    ffi::lua_rawgetp(state, ffi::LUA_REGISTRYINDEX, key);
}

static DESTRUCTED_USERDATA_METATABLE: u8 = 0;
static ERROR_PRINT_BUFFER_KEY: u8 = 0;
