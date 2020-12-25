use crate::lisp::LispObject;
use crate::multibyte::LispStringRef;
use crate::remacs_sys::{intern_c_string, make_string_from_utf8, Ffuncall};
use lazy_static::lazy_static;
use remacs_macros::lisp_fn;
use rusty_v8 as v8;
use std::convert::TryFrom;
use std::convert::TryInto;
use std::ffi::CString;

struct EmacsJsRuntime {
    r: Option<tokio::runtime::Runtime>,
    w: Option<deno_runtime::worker::MainWorker>,
}

static mut MAIN_RUNTIME: std::mem::MaybeUninit<EmacsJsRuntime> =
    std::mem::MaybeUninit::<EmacsJsRuntime>::uninit();

impl EmacsJsRuntime {
    fn set_main(r: tokio::runtime::Runtime, w: deno_runtime::worker::MainWorker) {
        let main = EmacsJsRuntime {
            r: Some(r),
            w: Some(w),
        };
        unsafe { MAIN_RUNTIME.write(main) };
    }

    fn runtime() -> &'static mut EmacsJsRuntime {
        unsafe { &mut *MAIN_RUNTIME.as_mut_ptr() }
    }

    fn take() -> (tokio::runtime::Runtime, deno_runtime::worker::MainWorker) {
        let runtime = Self::runtime();
        (runtime.r.take().unwrap(), runtime.w.take().unwrap())
    }
}

pub fn lisp_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let mut lisp_args = vec![];
    let len = args.length();

    let message = args
        .get(0)
        .to_string(scope)
        .unwrap()
        .to_rust_string_lossy(scope)
        .replace("_", "-");
    let cstr = CString::new(message).expect("Failure of CString");
    let interned = unsafe { intern_c_string(cstr.as_ptr()) };
    lisp_args.push(interned);

    for i in 1..len {
        let arg = args
            .get(i)
            .to_string(scope)
            .unwrap()
            .to_rust_string_lossy(scope);

        if let Ok(deser) = crate::parsing::deser(&arg, true) {
            lisp_args.push(deser);
        } else {
        }
    }

    let results = unsafe { Ffuncall(lisp_args.len().try_into().unwrap(), lisp_args.as_mut_ptr()) };
    // LOGIC, attempt to se, with a version of se that returns an error,
    // if this can't se, it is a proxy, and we will treat it as such.
    if let Ok(json) = crate::parsing::ser(results) {
        let r = v8::Local::<v8::Value>::try_from(v8::String::new(scope, &json).unwrap()).unwrap();
        retval.set(r);
    } else {
        // @TODO, FIXME
        // This is NOT how to implement proxies! Use the proper v8 API
        // for setting a real proxy.
        let obj = v8::Object::new(scope);
        let key = v8::String::new(scope, "__proxy__").unwrap();
        let value = v8::String::new(scope, &results.to_C_unsigned().to_string()).unwrap();
        obj.set(
            scope,
            v8::Local::<v8::Value>::try_from(key).unwrap(),
            v8::Local::<v8::Value>::try_from(value).unwrap(),
        );
        let json_result =
            v8::json::stringify(scope, v8::Local::<v8::Value>::try_from(obj).unwrap()).unwrap();
        let r = v8::Local::<v8::Value>::try_from(json_result).unwrap();
        retval.set(r);
    }
}

#[lisp_fn]
pub fn eval_js(string_obj: LispStringRef) -> LispObject {
    js_eval(string_obj.to_utf8())
}

#[lisp_fn]
pub fn eval_js_file(filename: LispStringRef) -> LispObject {
    let string = std::fs::read_to_string(filename.to_utf8()).unwrap();
    println!("{}", string);
    js_eval(string)
}

macro_rules! tick_js {
    ($r:expr, $worker:expr) => {{
        $r.block_on(async move {
            let _x: () = futures::future::poll_fn(|cx| {
                $worker.poll_event_loop(cx);
                std::task::Poll::Ready(())
            })
            .await;

            $worker
        })
    }};
}

fn js_eval(string: String) -> LispObject {
    let mut r = tokio::runtime::Builder::new()
        .threaded_scheduler()
        .enable_io()
        .enable_time()
        .max_threads(32)
        .build()
        .unwrap();

    let main_module = deno_core::ModuleSpecifier::resolve_url_or_path("./test.js").unwrap();
    let permissions = deno_runtime::permissions::Permissions::default();

    let options = deno_runtime::worker::WorkerOptions {
        apply_source_maps: false,
        user_agent: "x".to_string(),
        args: vec![],
        debug_flag: false,
        unstable: false,
        ca_filepath: None,
        seed: None,
        js_error_create_fn: None,
        create_web_worker_cb: std::sync::Arc::new(|_| unreachable!()),
        attach_inspector: false,
        maybe_inspector_server: None,
        should_break_on_first_statement: false,
        module_loader: std::rc::Rc::new(deno_core::FsModuleLoader),
        runtime_version: "x".to_string(),
        ts_version: "x".to_string(),
        no_color: true,
        get_error_class_fn: None,
    };

    let mut worker =
        deno_runtime::worker::MainWorker::from_options(main_module.clone(), permissions, &options);
    worker = r.block_on(async move {
        worker.bootstrap(&options);
        let runtime = &mut worker.js_runtime;
        {
            let context = runtime.global_context();
            let scope = &mut v8::HandleScope::with_context(runtime.v8_isolate(), context);
            let context = scope.get_current_context();
            let global = context.global(scope);
            let name = v8::String::new(scope, "lisp_invoke").unwrap();
            let func = v8::Function::new(scope, lisp_callback).unwrap();
            global.set(scope, name.into(), func.into());
        }
        {
            runtime
                .execute(
                    "prelim.js",
                    "var lisp = new Proxy({}, {
                get: function(o, k) {
                   return function() {
                       const modargs = [k.replaceAll('-', '_')];
                          for (let i = 0; i < arguments.length; ++i) {
                             modargs.push(JSON.stringify(arguments[i]));
                          }
                       return JSON.parse(lisp_invoke.apply(this, modargs));
                   }

                }});",
                )
                .unwrap();
        }

        worker.execute_module(&main_module).await.unwrap();
        worker
    });

    EmacsJsRuntime::set_main(r, worker);
    //(run-with-timer t 1 'js-tick-event-loop)

    let cstr = CString::new("run-with-timer").expect("Failed to create timer");
    let callback = CString::new("js-tick-event-loop").expect("Failed to create timer");
    unsafe {
        let fun = crate::remacs_sys::intern_c_string(cstr.as_ptr());
        let fun_callback = crate::remacs_sys::intern_c_string(callback.as_ptr());
        let mut args = vec![
            fun,
            crate::remacs_sys::Qt,
            crate::remacs_sys::make_int(1),
            fun_callback,
        ];
        crate::remacs_sys::Ffuncall(args.len().try_into().unwrap(), args.as_mut_ptr());
    }

    crate::remacs_sys::Qnil
}

#[lisp_fn]
pub fn js_tick_event_loop() -> LispObject {
    let (mut r, mut w) = EmacsJsRuntime::take();
    w = tick_js!(r, w);
    EmacsJsRuntime::set_main(r, w);
    crate::remacs_sys::Qnil
}

include!(concat!(env!("OUT_DIR"), "/javascript_exports.rs"));
