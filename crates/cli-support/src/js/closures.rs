//! Support for closures in wasm-bindgen
//!
//! This module contains the bulk of the support necessary to support closures
//! in `wasm-bindgen`. The main "support" here is that `Closure::wrap` creates
//! a `JsValue` through... well... unconventional mechanisms.
//!
//! This module contains one public function, `rewrite`. The function will
//! rewrite the wasm module to correctly call closure factories and thread
//! through values into the final `Closure` object. More details about how all
//! this works can be found in the code below.

use crate::descriptor::{ClosureKind, Descriptor};
use crate::js::js2rust::Js2Rust;
use crate::js::Context;
use failure::Error;
use std::collections::{BTreeMap, HashSet};
use std::mem;
use walrus::ir::{Expr, ExprId};
use walrus::{FunctionId, LocalFunction};

pub fn rewrite(input: &mut Context) -> Result<(), Error> {
    let info = ClosureDescriptors::new(input);

    if info.element_removal_list.len() == 0 {
        return Ok(());
    }

    info.delete_function_table_entries(input);
    info.inject_imports(input)?;
    Ok(())
}

#[derive(Default)]
struct ClosureDescriptors {
    /// A list of elements to remove from the function table. The first element
    /// of the pair is the index of the entry in the element section, and the
    /// second element of the pair is the index within that entry to remove.
    element_removal_list: HashSet<usize>,

    /// A map from local functions which contain calls to
    /// `__wbindgen_describe_closure` to the information about the closure
    /// descriptor it contains.
    ///
    /// This map is later used to replace all calls to the keys of this map with
    /// calls to the value of the map.
    func_to_descriptor: BTreeMap<FunctionId, DescribeInstruction>,
}

struct DescribeInstruction {
    call: ExprId,
    descriptor: Descriptor,
}

impl ClosureDescriptors {
    /// Find all invocations of `__wbindgen_describe_closure`.
    ///
    /// We'll be rewriting all calls to functions who call this import. Here we
    /// iterate over all code found in the module, and anything which calls our
    /// special imported function is interpreted.  The result of interpretation will
    /// inform of us of an entry to remove from the function table (as the describe
    /// function is never needed at runtime) as well as a `Descriptor` which
    /// describes the type of closure needed.
    ///
    /// All this information is then returned in the `ClosureDescriptors` return
    /// value.
    fn new(input: &mut Context) -> ClosureDescriptors {
        use walrus::ir::*;

        let wbindgen_describe_closure = match input.interpreter.describe_closure_id() {
            Some(i) => i,
            None => return Default::default(),
        };
        let mut ret = ClosureDescriptors::default();

        for (id, local) in input.module.funcs.iter_local() {
            let entry = local.entry_block();
            let mut find = FindDescribeClosure {
                func: local,
                wbindgen_describe_closure,
                cur: entry.into(),
                call: None,
            };
            find.visit_block_id(&entry);
            if let Some(call) = find.call {
                let descriptor = input
                    .interpreter
                    .interpret_closure_descriptor(id, input.module, &mut ret.element_removal_list)
                    .unwrap();
                ret.func_to_descriptor.insert(
                    id,
                    DescribeInstruction {
                        call,
                        descriptor: Descriptor::decode(descriptor),
                    },
                );
            }
        }

        return ret;

        struct FindDescribeClosure<'a> {
            func: &'a LocalFunction,
            wbindgen_describe_closure: FunctionId,
            cur: ExprId,
            call: Option<ExprId>,
        }

        impl<'a> Visitor<'a> for FindDescribeClosure<'a> {
            fn local_function(&self) -> &'a LocalFunction {
                self.func
            }

            fn visit_expr_id(&mut self, id: &ExprId) {
                let prev = mem::replace(&mut self.cur, *id);
                id.visit(self);
                self.cur = prev;
            }

            fn visit_call(&mut self, call: &Call) {
                call.visit(self);
                if call.func == self.wbindgen_describe_closure {
                    assert!(self.call.is_none());
                    self.call = Some(self.cur);
                }
            }
        }
    }

    /// Here we remove elements from the function table. All our descriptor
    /// functions are entries in this function table and can be removed once we
    /// use them as they're not actually needed at runtime.
    ///
    /// One option for removal is to replace the function table entry with an
    /// index to a dummy function, but for now we simply remove the table entry
    /// altogether by splitting the section and having multiple `elem` sections
    /// with holes in them.
    fn delete_function_table_entries(&self, input: &mut Context) {
        let table_id = match input.interpreter.function_table_id() {
            Some(id) => id,
            None => return,
        };
        let table = input.module.tables.get_mut(table_id);
        let table = match &mut table.kind {
            walrus::TableKind::Function(f) => f,
            walrus::TableKind::Anyref(_) => unreachable!(),
        };
        for idx in self.element_removal_list.iter().cloned() {
            log::trace!("delete element {}", idx);
            assert!(table.elements[idx].is_some());
            table.elements[idx] = None;
        }
    }

    /// Inject new imports into the module.
    ///
    /// This function will inject new imported functions into the `input` module
    /// described by the fields internally. These new imports will be closure
    /// factories and are freshly generated shim in JS.
    fn inject_imports(&self, input: &mut Context) -> Result<(), Error> {
        let wbindgen_describe_closure = match input.interpreter.describe_closure_id() {
            Some(i) => i,
            None => return Ok(()),
        };

        // We'll be injecting new imports and we'll need to give them all a
        // type. The signature is all `(i32, i32, i32) -> i32` currently
        let ty = input.module.funcs.get(wbindgen_describe_closure).ty();

        // For all our descriptors we found we inject a JS shim for the
        // descriptor. This JS shim will manufacture a JS `function`, and
        // prepare it to be invoked.
        //
        // Once all that's said and done we inject a new import into the wasm
        // module of our new wrapper, and then rewrite the appropriate call
        // instruction.
        for (func, instr) in self.func_to_descriptor.iter() {
            let import_name = format!("__wbindgen_closure_wrapper{}", func.index());

            let closure = instr.descriptor.closure().unwrap();

            let (js, _ts, _js_doc) = {
                let mut builder = Js2Rust::new("", input);
                builder
                    .prelude("this.cnt++;\n")
                    .prelude("const a = this.a;\n")
                    .rust_argument("a")
                    .rust_argument("b");
                if closure.kind == ClosureKind::FnMut || closure.kind == ClosureKind::FnOnce {
                    // The function is not re-entrant, so zero out `a`.
                    builder.prelude("this.a = 0;\n");
                    if closure.kind == ClosureKind::FnMut {
                        // The function can be called again after it returns, so
                        // restor `a` again.
                        builder.finally("this.a = a;\n");
                    }
                }
                if closure.kind != ClosureKind::FnOnce {
                    builder.finally("if (this.cnt-- == 1) d(a, b);");
                }
                builder.process(&closure.function)?.finish("function", "f")
            };
            input.expose_add_heap_object();
            input.function_table_needed = true;
            let body = format!(
                "function(a, b, _ignored) {{
                    const f = wasm.__wbg_function_table.get({});
                    const d = wasm.__wbg_function_table.get({});
                    const cb = {};
                    cb.a = a;
                    cb.cnt = 1;
                    let real = cb.bind(cb);
                    real.original = cb;
                    return addHeapObject(real);
                }}",
                closure.shim_idx, closure.dtor_idx, js,
            );
            input.export(&import_name, &body, None);

            let id = input
                .module
                .add_import_func("__wbindgen_placeholder__", &import_name, ty);

            let local = match &mut input.module.funcs.get_mut(*func).kind {
                walrus::FunctionKind::Local(l) => l,
                _ => unreachable!(),
            };
            match local.get_mut(instr.call) {
                Expr::Call(e) => {
                    assert_eq!(e.func, wbindgen_describe_closure);
                    e.func = id;
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }
}
