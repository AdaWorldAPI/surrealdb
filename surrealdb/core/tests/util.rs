// `util.rs` is included as a `mod util;` submodule by sibling test files
// (e.g. `remove.rs`), not as a crate root. The
// `#![recursion_limit = "1024"]` attribute must therefore live on the
// parent test binary's root file, not here — Rust 1.95 rejects this
// inner attribute when the file is loaded as a module rather than the
// crate root. Each test binary that needs the higher recursion limit
// already declares it on its own first line.

#[expect(unused_macros)]
macro_rules! assert_empty_val {
	($tx:expr, $key:expr) => {{
		let r = $tx.get($key).await?;
		assert!(r.is_none());
	}};
}

#[expect(unused_macros)]
macro_rules! assert_empty_prefix {
	($tx:expr, $rng:expr) => {{
		let r = $tx.getp($rng, None).await?;
		assert!(r.is_empty());
	}};
}

#[expect(unused_macros)]
macro_rules! assert_empty_range {
	($tx:expr, $rng:expr) => {{
		let r = $tx.getr($rng).await?;
		assert!(r.is_empty());
	}};
}
