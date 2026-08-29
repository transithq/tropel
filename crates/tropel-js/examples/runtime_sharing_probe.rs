//! TR-503 probe: does sharing one QuickJS `Runtime` across VU contexts save
//! memory, and how much?
//!
//! The original TR-503 asserted "57 KB/VU, -92.3%" without measuring anything;
//! the code it cited never shared a Runtime at all. This measures the two
//! arrangements directly before any of it is built for real.
//!
//! Both arms load the SAME shim source into each context, because the whole
//! question is whether QuickJS's per-Runtime atom and shape tables are shared
//! when many contexts parse identical source.
//!
//!   cargo run --release -p tropel-js --example runtime_sharing_probe

use rquickjs::{Context, Runtime};

/// Roughly the shape of the real shim bundle: a lot of distinct identifiers
/// and string literals, which is precisely what lands in the atom table.
fn shim_source() -> String {
    let mut s = String::new();
    for i in 0..400 {
        s.push_str(&format!(
            "globalThis.__probe_fn_{i} = function probeFn{i}(argAlpha{i}, argBeta{i}) {{\n\
               const localGamma{i} = 'string-literal-value-number-{i}';\n\
               const localDelta{i} = {{ keyOne{i}: 1, keyTwo{i}: 2, keyThree{i}: localGamma{i} }};\n\
               return argAlpha{i} + argBeta{i} + localDelta{i}.keyOne{i};\n\
             }};\n"
        ));
    }
    s
}

fn main() {
    const N: usize = 50;
    let src = shim_source();
    println!("shim source: {} bytes, {} contexts per arm\n", src.len(), N);

    // ---- Arm A: one Runtime per context (what ships today) ----
    let mut own: Vec<(Runtime, Context)> = Vec::with_capacity(N);
    let mut own_total = 0usize;
    for _ in 0..N {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|c| {
            c.eval::<(), _>(src.as_bytes()).expect("eval shim");
        });
        own_total = rt.memory_usage().malloc_size as usize;
        own.push((rt, ctx));
    }
    // Sum across all runtimes: each accounts only for itself.
    let own_sum: usize = own
        .iter()
        .map(|(rt, _)| rt.memory_usage().malloc_size as usize)
        .sum();

    // ---- Arm B: one shared Runtime, N contexts on it ----
    let shared_rt = Runtime::new().expect("runtime");
    let mut shared: Vec<Context> = Vec::with_capacity(N);
    for _ in 0..N {
        let ctx = Context::full(&shared_rt).expect("context");
        ctx.with(|c| {
            c.eval::<(), _>(src.as_bytes()).expect("eval shim");
        });
        shared.push(ctx);
    }
    let shared_sum = shared_rt.memory_usage().malloc_size as usize;

    println!(
        "per-VU Runtime  : {own_sum:>10} B total  ({:>8} B/context)",
        own_sum / N
    );
    println!(
        "shared Runtime  : {shared_sum:>10} B total  ({:>8} B/context)",
        shared_sum / N
    );
    let saved = own_sum as i64 - shared_sum as i64;
    println!(
        "\nsaving          : {saved:>10} B total  ({:>8} B/context, {:.1}%)",
        saved / N as i64,
        (saved as f64 / own_sum as f64) * 100.0
    );
    println!("last single rt  : {own_total} B (one context's own runtime)");
    std::hint::black_box((own, shared));
}
