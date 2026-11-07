//! Standalone Linux feasibility probe. Not a production or OS sandbox.
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::promise::{MaybePromise, PromiseState};
use rquickjs::{Context, Ctx, Error, Function, Module, Promise, Runtime, Value};
use serde_json::{Value as Json, json};
use std::io::Read;
use std::time::{Duration, Instant};

const REQUEST_MAX: usize = 256 * 1024;
const SOURCE_MAX: usize = 64 * 1024;
const INPUT_MAX: usize = 64 * 1024;
const OUTPUT_MAX: usize = 64 * 1024;
const JOB_MAX: usize = 10_000;
struct DenyImports;
impl Resolver for DenyImports {
    fn resolve<'js>(
        &mut self,
        _: &Ctx<'js>,
        base: &str,
        name: &str,
        _: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        Err(Error::new_resolving(base, name))
    }
}
impl Loader for DenyImports {
    fn load<'js>(
        &mut self,
        _: &Ctx<'js>,
        name: &str,
        _: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        Err(Error::new_loading(name))
    }
}
fn settle<'js>(
    ctx: &Ctx<'js>,
    promise: MaybePromise<'js>,
    deadline: Instant,
    jobs: &mut usize,
) -> Result<Value<'js>, &'static str> {
    loop {
        if Instant::now() >= deadline {
            return Err("deadline");
        }
        match promise.state() {
            PromiseState::Resolved | PromiseState::Rejected => {
                return promise
                    .result::<Value>()
                    .ok_or("promise_pending")?
                    .map_err(|_| "js_exception");
            }
            PromiseState::Pending => {
                if *jobs >= JOB_MAX {
                    return Err("job_limit");
                }
                *jobs += 1;
                // Context already owns the runtime lock: do not use Runtime::execute_pending_job.
                // This API doesn't expose unrelated job errors; target-promise state is authoritative.
                if !ctx.execute_pending_job() {
                    return Err("promise_pending");
                }
            }
        }
    }
}
fn execute(source: &str, input: &str) -> Result<Json, &'static str> {
    let runtime = Runtime::new().map_err(|_| "runtime_init")?;
    runtime.set_memory_limit(32 * 1024 * 1024);
    runtime.set_max_stack_size(256 * 1024);
    let deadline = Instant::now() + Duration::from_millis(250);
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
    runtime.set_loader(DenyImports, DenyImports);
    let context = Context::full(&runtime).map_err(|_| "context_init")?;
    let answer = context.with(|ctx| -> Result<Json, &'static str> {
        // Capture native Promise.resolve before plugin code can replace global bindings.
        let normalize: Function = ctx
            .eval("Promise.resolve.bind(Promise)")
            .map_err(|_| "intrinsic_init")?;
        let (module, evaluated) = Module::declare(ctx.clone(), "entry.js", source)
            .and_then(|m| m.eval())
            .map_err(|_| "module_error")?;
        let mut jobs = 0;
        settle(
            &ctx,
            MaybePromise::from_value(evaluated.into_value()),
            deadline,
            &mut jobs,
        )?;
        let run: Function = module.get("run").map_err(|_| "run_missing")?;
        let argument = ctx.json_parse(input.as_bytes()).map_err(|_| "input_json")?;
        let raw: Value = run.call((argument,)).map_err(|_| "run_error")?;
        let promise: Promise = normalize.call((raw,)).map_err(|_| "run_error")?;
        let value = settle(
            &ctx,
            MaybePromise::from_value(promise.into_value()),
            deadline,
            &mut jobs,
        )?;
        let encoded = ctx
            .json_stringify(value)
            .map_err(|_| "serialization_error")?
            .ok_or("non_json")?
            .to_string()
            .map_err(|_| "serialization_error")?;
        if encoded.len() > OUTPUT_MAX {
            return Err("output_limit");
        }
        serde_json::from_str(&encoded).map_err(|_| "non_json")
    });
    // Never inspect arbitrary JS exception objects: formatting could execute getters or toJSON.
    // Native deadline remains installed through module evaluation, jobs and serialization.
    if Instant::now() >= deadline {
        Err("deadline")
    } else {
        answer
    }
}
fn request(input: impl Read) -> Result<Json, &'static str> {
    let mut bytes = Vec::new();
    input
        .take((REQUEST_MAX + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "input_read")?;
    if bytes.len() > REQUEST_MAX {
        return Err("request_limit");
    }
    let request: Json = serde_json::from_slice(&bytes).map_err(|_| "invalid_request")?;
    let source = request.get("source").and_then(Json::as_str).ok_or("invalid_request")?;
    if source.len() > SOURCE_MAX {
        return Err("source_limit");
    }
    let input = serde_json::to_string(request.get("input").ok_or("invalid_request")?).map_err(|_| "input_json")?;
    if input.len() > INPUT_MAX {
        return Err("input_limit");
    }
    execute(source, &input)
}
pub(super) fn response(input: impl Read) -> Json {
    match request(input) {
        Ok(result) => json!({"ok": true, "result": result}),
        Err(error) => json!({"ok": false, "error": error}),
    }
}
