// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Proc macro of Support code for the runtime.

#![recursion_limit = "512"]

mod benchmark;
mod construct_runtime;
mod crate_version;
mod derive_impl;
mod dummy_part_checker;
mod key_prefix;
mod match_and_insert;
mod no_bound;
mod pallet;
mod pallet_error;
mod storage_alias;
mod transactional;
mod tt_macro;

use frame_support_procedural_tools::generate_access_from_frame_or_crate;
use macro_magic::{import_tokens_attr, import_tokens_attr_verbatim};
use proc_macro::TokenStream;
use quote::{quote, ToTokens};
use std::{cell::RefCell, str::FromStr};
use syn::{parse_macro_input, Error, ItemImpl, ItemMod, TraitItemType};

pub(crate) const INHERENT_INSTANCE_NAME: &str = "__InherentHiddenInstance";

thread_local! {
	/// A global counter, can be used to generate a relatively unique identifier.
	static COUNTER: RefCell<Counter> = RefCell::new(Counter(0));
}

/// Counter to generate a relatively unique identifier for macros. This is necessary because
/// declarative macros gets hoisted to the crate root, which shares the namespace with other pallets
/// containing the very same macros.
struct Counter(u64);

impl Counter {
	fn inc(&mut self) -> u64 {
		let ret = self.0;
		self.0 += 1;
		ret
	}
}

/// Get the value from the given environment variable set by cargo.
///
/// The value is parsed into the requested destination type.
fn get_cargo_env_var<T: FromStr>(version_env: &str) -> std::result::Result<T, ()> {
	let version = std::env::var(version_env)
		.unwrap_or_else(|_| panic!("`{}` is always set by cargo; qed", version_env));

	T::from_str(&version).map_err(drop)
}

/// Generate the counter_prefix related to the storage.
/// counter_prefix is used by counted storage map.
fn counter_prefix(prefix: &str) -> String {
	format!("CounterFor{}", prefix)
}

/// Construct a runtime, with the given name and the given pallets.
///
/// The parameters here are specific types for `Block`, `NodeBlock`, and `UncheckedExtrinsic`
/// and the pallets that are used by the runtime.
/// `Block` is the block type that is used in the runtime and `NodeBlock` is the block type
/// that is used in the node. For instance they can differ in the extrinsics type.
///
/// # Example:
///
/// ```ignore
/// construct_runtime!(
///     pub enum Runtime where
///         Block = Block,
///         NodeBlock = node::Block,
///         UncheckedExtrinsic = UncheckedExtrinsic
///     {
///         System: frame_system::{Pallet, Call, Event<T>, Config<T>} = 0,
///         Test: path::to::test::{Pallet, Call} = 1,
///
///         // Pallets with instances.
///         Test2_Instance1: test2::<Instance1>::{Pallet, Call, Storage, Event<T, I>, Config<T, I>, Origin<T, I>},
///         Test2_DefaultInstance: test2::{Pallet, Call, Storage, Event<T>, Config<T>, Origin<T>} = 4,
///
///         // Pallets declared with `pallet` attribute macro: no need to define the parts
///         Test3_Instance1: test3::<Instance1>,
///         Test3_DefaultInstance: test3,
///
///         // with `exclude_parts` keyword some part can be excluded.
///         Test4_Instance1: test4::<Instance1> exclude_parts { Call, Origin },
///         Test4_DefaultInstance: test4 exclude_parts { Storage },
///
///         // with `use_parts` keyword, a subset of the pallet parts can be specified.
///         Test4_Instance1: test4::<Instance1> use_parts { Pallet, Call},
///         Test4_DefaultInstance: test4 use_parts { Pallet },
///     }
/// )
/// ```
///
/// Each pallet is declared as such:
/// * `Identifier`: name given to the pallet that uniquely identifies it.
///
/// * `:`: colon separator
///
/// * `path::to::pallet`: identifiers separated by colons which declare the path to a pallet
///   definition.
///
/// * `::<InstanceN>` optional: specify the instance of the pallet to use. If not specified it will
///   use the default instance (or the only instance in case of non-instantiable pallets).
///
/// * `::{ Part1, Part2<T>, .. }` optional if pallet declared with `frame_support::pallet`: Comma
///   separated parts declared with their generic. If a pallet is declared with
///   `frame_support::pallet` macro then the parts can be automatically derived if not explicitly
///   provided. We provide support for the following module parts in a pallet:
///
///   - `Pallet` - Required for all pallets
///   - `Call` - If the pallet has callable functions
///   - `Storage` - If the pallet uses storage
///   - `Event` or `Event<T>` (if the event is generic) - If the pallet emits events
///   - `Origin` or `Origin<T>` (if the origin is generic) - If the pallet has instanciable origins
///   - `Config` or `Config<T>` (if the config is generic) - If the pallet builds the genesis
///     storage with `GenesisConfig`
///   - `Inherent` - If the pallet provides/can check inherents.
///   - `ValidateUnsigned` - If the pallet validates unsigned extrinsics.
///
///   It is important to list these parts here to export them correctly in the metadata or to make
/// the pallet usable in the runtime.
///
/// * `exclude_parts { Part1, Part2 }` optional: comma separated parts without generics. I.e. one of
///   `Pallet`, `Call`, `Storage`, `Event`, `Origin`, `Config`, `Inherent`, `ValidateUnsigned`. It
///   is incompatible with `use_parts`. This specifies the part to exclude. In order to select
///   subset of the pallet parts.
///
///   For example excluding the part `Call` can be useful if the runtime doesn't want to make the
///   pallet calls available.
///
/// * `use_parts { Part1, Part2 }` optional: comma separated parts without generics. I.e. one of
///   `Pallet`, `Call`, `Storage`, `Event`, `Origin`, `Config`, `Inherent`, `ValidateUnsigned`. It
///   is incompatible with `exclude_parts`. This specifies the part to use. In order to select a
///   subset of the pallet parts.
///
///   For example not using the part `Call` can be useful if the runtime doesn't want to make the
///   pallet calls available.
///
/// * `= $n` optional: number to define at which index the pallet variants in `OriginCaller`, `Call`
///   and `Event` are encoded, and to define the ModuleToIndex value.
///
///   if `= $n` is not given, then index is resolved in the same way as fieldless enum in Rust
///   (i.e. incrementedly from previous index):
///   ```nocompile
///   pallet1 .. = 2,
///   pallet2 .., // Here pallet2 is given index 3
///   pallet3 .. = 0,
///   pallet4 .., // Here pallet4 is given index 1
///   ```
///
/// # Note
///
/// The population of the genesis storage depends on the order of pallets. So, if one of your
/// pallets depends on another pallet, the pallet that is depended upon needs to come before
/// the pallet depending on it.
///
/// # Type definitions
///
/// * The macro generates a type alias for each pallet to their `Pallet`. E.g. `type System =
///   frame_system::Pallet<Runtime>`
#[proc_macro]
pub fn construct_runtime(input: TokenStream) -> TokenStream {
	construct_runtime::construct_runtime(input)
}

/// The pallet struct placeholder `#[pallet::pallet]` is mandatory and allows you to specify
/// pallet information.
///
/// The struct must be defined as follows:
/// ```ignore
/// #[pallet::pallet]
/// pub struct Pallet<T>(_);
/// ```
/// I.e. a regular struct definition named `Pallet`, with generic T and no where clause.
///
/// ## Macro expansion:
///
/// The macro adds this attribute to the struct definition:
/// ```ignore
/// #[derive(
/// 	frame_support::CloneNoBound,
/// 	frame_support::EqNoBound,
/// 	frame_support::PartialEqNoBound,
/// 	frame_support::RuntimeDebugNoBound,
/// )]
/// ```
/// and replaces the type `_` with `PhantomData<T>`. It also implements on the pallet:
/// * `GetStorageVersion`
/// * `OnGenesis`: contains some logic to write the pallet version into storage.
/// * `PalletErrorTypeInfo`: provides the type information for the pallet error, if defined.
///
/// It declares `type Module` type alias for `Pallet`, used by `construct_runtime`.
///
/// It implements `PalletInfoAccess` on `Pallet` to ease access to pallet information given by
/// `frame_support::traits::PalletInfo`. (The implementation uses the associated type
/// `frame_system::Config::PalletInfo`).
///
/// It implements `StorageInfoTrait` on `Pallet` which give information about all storages.
///
/// If the attribute `generate_store` is set then the macro creates the trait `Store` and
/// implements it on `Pallet`.
///
/// If the attribute `set_storage_max_encoded_len` is set then the macro calls
/// `StorageInfoTrait` for each storage in the implementation of `StorageInfoTrait` for the
/// pallet. Otherwise it implements `StorageInfoTrait` for the pallet using the
/// `PartialStorageInfoTrait` implementation of storages.
///
/// ## Dev Mode (`#[pallet(dev_mode)]`)
///
/// Specifying the argument `dev_mode` will allow you to enable dev mode for a pallet. The aim
/// of dev mode is to loosen some of the restrictions and requirements placed on production
/// pallets for easy tinkering and development. Dev mode pallets should not be used in
/// production. Enabling dev mode has the following effects:
///
/// * Weights no longer need to be specified on every `#[pallet::call]` declaration. By default, dev
///   mode pallets will assume a weight of zero (`0`) if a weight is not specified. This is
///   equivalent to specifying `#[weight(0)]` on all calls that do not specify a weight.
/// * Call indices no longer need to be specified on every `#[pallet::call]` declaration. By
///   default, dev mode pallets will assume a call index based on the order of the call.
/// * All storages are marked as unbounded, meaning you do not need to implement `MaxEncodedLen` on
///   storage types. This is equivalent to specifying `#[pallet::unbounded]` on all storage type
///   definitions.
/// * Storage hashers no longer need to be specified and can be replaced by `_`. In dev mode, these
///   will be replaced by `Blake2_128Concat`. In case of explicit key-binding, `Hasher` can simply
///   be ignored when in `dev_mode`.
///
/// Note that the `dev_mode` argument can only be supplied to the `#[pallet]` or
/// `#[frame_support::pallet]` attribute macro that encloses your pallet module. This argument
/// cannot be specified anywhere else, including but not limited to the `#[pallet::pallet]`
/// attribute macro.
///
/// <div class="example-wrap" style="display:inline-block"><pre class="compile_fail"
/// style="white-space:normal;font:inherit;">
/// <strong>WARNING</strong>:
/// You should not deploy or use dev mode pallets in production. Doing so can break your chain
/// and therefore should never be done. Once you are done tinkering, you should remove the
/// 'dev_mode' argument from your #[pallet] declaration and fix any compile errors before
/// attempting to use your pallet in a production scenario.
/// </pre></div>
///
/// See `frame_support::pallet` docs for more info.
///
/// ## Runtime Metadata Documentation
///
/// The documentation added to this pallet is included in the runtime metadata.
///
/// The documentation can be defined in the following ways:
///
/// ```ignore
/// #[pallet::pallet]
/// /// Documentation for pallet 1
/// #[doc = "Documentation for pallet 2"]
/// #[doc = include_str!("../README.md")]
/// #[pallet_doc("../doc1.md")]
/// #[pallet_doc("../doc2.md")]
/// pub mod pallet {}
/// ```
///
/// The runtime metadata for this pallet contains the following
///  - " Documentation for pallet 1" (captured from `///`)
///  - "Documentation for pallet 2"  (captured from `#[doc]`)
///  - content of ../README.md       (captured from `#[doc]` with `include_str!`)
///  - content of "../doc1.md"       (captured from `pallet_doc`)
///  - content of "../doc2.md"       (captured from `pallet_doc`)
///
/// ### `doc` attribute
///
/// The value of the `doc` attribute is included in the runtime metadata, as well as
/// expanded on the pallet module. The previous example is expanded to:
///
/// ```ignore
/// /// Documentation for pallet 1
/// /// Documentation for pallet 2
/// /// Content of README.md
/// pub mod pallet {}
/// ```
///
/// If you want to specify the file from which the documentation is loaded, you can use the
/// `include_str` macro. However, if you only want the documentation to be included in the
/// runtime metadata, use the `pallet_doc` attribute.
///
/// ### `pallet_doc` attribute
///
/// Unlike the `doc` attribute, the documentation provided to the `pallet_doc` attribute is
/// not inserted on the module.
///
/// The `pallet_doc` attribute can only be provided with one argument,
/// which is the file path that holds the documentation to be added to the metadata.
///
/// This approach is beneficial when you use the `include_str` macro at the beginning of the file
/// and want that documentation to extend to the runtime metadata, without reiterating the
/// documentation on the pallet module itself.
#[proc_macro_attribute]
pub fn pallet(attr: TokenStream, item: TokenStream) -> TokenStream {
	pallet::pallet(attr, item)
}

/// An attribute macro that can be attached to a (non-empty) module declaration. Doing so will
/// designate that module as a benchmarking module.
///
/// See `frame_benchmarking::v2` for more info.
#[proc_macro_attribute]
pub fn benchmarks(attr: TokenStream, tokens: TokenStream) -> TokenStream {
	match benchmark::benchmarks(attr, tokens, false) {
		Ok(tokens) => tokens,
		Err(err) => err.to_compile_error().into(),
	}
}

/// An attribute macro that can be attached to a (non-empty) module declaration. Doing so will
/// designate that module as an instance benchmarking module.
///
/// See `frame_benchmarking::v2` for more info.
#[proc_macro_attribute]
pub fn instance_benchmarks(attr: TokenStream, tokens: TokenStream) -> TokenStream {
	match benchmark::benchmarks(attr, tokens, true) {
		Ok(tokens) => tokens,
		Err(err) => err.to_compile_error().into(),
	}
}

/// An attribute macro used to declare a benchmark within a benchmarking module. Must be
/// attached to a function definition containing an `#[extrinsic_call]` or `#[block]`
/// attribute.
///
/// See `frame_benchmarking::v2` for more info.
#[proc_macro_attribute]
pub fn benchmark(_attrs: TokenStream, _tokens: TokenStream) -> TokenStream {
	quote!(compile_error!(
		"`#[benchmark]` must be in a module labeled with #[benchmarks] or #[instance_benchmarks]."
	))
	.into()
}

/// An attribute macro used to specify the extrinsic call inside a benchmark function, and also
/// used as a boundary designating where the benchmark setup code ends, and the benchmark
/// verification code begins.
///
/// See `frame_benchmarking::v2` for more info.
#[proc_macro_attribute]
pub fn extrinsic_call(_attrs: TokenStream, _tokens: TokenStream) -> TokenStream {
	quote!(compile_error!(
		"`#[extrinsic_call]` must be in a benchmark function definition labeled with `#[benchmark]`."
	);)
	.into()
}

/// An attribute macro used to specify that a block should be the measured portion of the
/// enclosing benchmark function, This attribute is also used as a boundary designating where
/// the benchmark setup code ends, and the benchmark verification code begins.
///
/// See `frame_benchmarking::v2` for more info.
#[proc_macro_attribute]
pub fn block(_attrs: TokenStream, _tokens: TokenStream) -> TokenStream {
	quote!(compile_error!(
		"`#[block]` must be in a benchmark function definition labeled with `#[benchmark]`."
	))
	.into()
}

/// Execute the annotated function in a new storage transaction.
///
/// The return type of the annotated function must be `Result`. All changes to storage performed
/// by the annotated function are discarded if it returns `Err`, or committed if `Ok`.
///
/// # Example
///
/// ```nocompile
/// #[transactional]
/// fn value_commits(v: u32) -> result::Result<u32, &'static str> {
/// 	Value::set(v);
/// 	Ok(v)
/// }
///
/// #[transactional]
/// fn value_rollbacks(v: u32) -> result::Result<u32, &'static str> {
/// 	Value::set(v);
/// 	Err("nah")
/// }
/// ```
#[proc_macro_attribute]
pub fn transactional(attr: TokenStream, input: TokenStream) -> TokenStream {
	transactional::transactional(attr, input).unwrap_or_else(|e| e.to_compile_error().into())
}

#[proc_macro_attribute]
pub fn require_transactional(attr: TokenStream, input: TokenStream) -> TokenStream {
	transactional::require_transactional(attr, input)
		.unwrap_or_else(|e| e.to_compile_error().into())
}

/// Derive [`Clone`] but do not bound any generic. Docs are at `frame_support::CloneNoBound`.
#[proc_macro_derive(CloneNoBound)]
pub fn derive_clone_no_bound(input: TokenStream) -> TokenStream {
	no_bound::clone::derive_clone_no_bound(input)
}

/// Derive [`Debug`] but do not bound any generics. Docs are at `frame_support::DebugNoBound`.
#[proc_macro_derive(DebugNoBound)]
pub fn derive_debug_no_bound(input: TokenStream) -> TokenStream {
	no_bound::debug::derive_debug_no_bound(input)
}

/// Derive [`Debug`], if `std` is enabled it uses `frame_support::DebugNoBound`, if `std` is not
/// enabled it just returns `"<wasm:stripped>"`.
/// This behaviour is useful to prevent bloating the runtime WASM blob from unneeded code.
#[proc_macro_derive(RuntimeDebugNoBound)]
pub fn derive_runtime_debug_no_bound(input: TokenStream) -> TokenStream {
	if cfg!(any(feature = "std", feature = "try-runtime")) {
		no_bound::debug::derive_debug_no_bound(input)
	} else {
		let input: syn::DeriveInput = match syn::parse(input) {
			Ok(input) => input,
			Err(e) => return e.to_compile_error().into(),
		};

		let name = &input.ident;
		let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

		quote::quote!(
			const _: () = {
				impl #impl_generics ::core::fmt::Debug for #name #ty_generics #where_clause {
					fn fmt(&self, fmt: &mut ::core::fmt::Formatter) -> core::fmt::Result {
						fmt.write_str("<wasm:stripped>")
					}
				}
			};
		)
		.into()
	}
}

/// Derive [`PartialEq`] but do not bound any generic. Docs are at
/// `frame_support::PartialEqNoBound`.
#[proc_macro_derive(PartialEqNoBound)]
pub fn derive_partial_eq_no_bound(input: TokenStream) -> TokenStream {
	no_bound::partial_eq::derive_partial_eq_no_bound(input)
}

/// derive Eq but do no bound any generic. Docs are at `frame_support::EqNoBound`.
#[proc_macro_derive(EqNoBound)]
pub fn derive_eq_no_bound(input: TokenStream) -> TokenStream {
	let input: syn::DeriveInput = match syn::parse(input) {
		Ok(input) => input,
		Err(e) => return e.to_compile_error().into(),
	};

	let name = &input.ident;
	let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

	quote::quote_spanned!(name.span() =>
		const _: () = {
			impl #impl_generics ::core::cmp::Eq for #name #ty_generics #where_clause {}
		};
	)
	.into()
}

/// derive `Default` but do no bound any generic. Docs are at `frame_support::DefaultNoBound`.
#[proc_macro_derive(DefaultNoBound, attributes(default))]
pub fn derive_default_no_bound(input: TokenStream) -> TokenStream {
	no_bound::default::derive_default_no_bound(input)
}

#[proc_macro]
pub fn crate_to_crate_version(input: TokenStream) -> TokenStream {
	crate_version::crate_to_crate_version(input)
		.unwrap_or_else(|e| e.to_compile_error())
		.into()
}

/// The number of module instances supported by the runtime, starting at index 1,
/// and up to `NUMBER_OF_INSTANCE`.
pub(crate) const NUMBER_OF_INSTANCE: u8 = 16;

/// This macro is meant to be used by frame-support only.
/// It implements the trait `HasKeyPrefix` and `HasReversibleKeyPrefix` for tuple of `Key`.
#[proc_macro]
pub fn impl_key_prefix_for_tuples(input: TokenStream) -> TokenStream {
	key_prefix::impl_key_prefix_for_tuples(input)
		.unwrap_or_else(syn::Error::into_compile_error)
		.into()
}

/// Internal macro use by frame_support to generate dummy part checker for old pallet declaration
#[proc_macro]
pub fn __generate_dummy_part_checker(input: TokenStream) -> TokenStream {
	dummy_part_checker::generate_dummy_part_checker(input)
}

/// Macro that inserts some tokens after the first match of some pattern.
///
/// # Example:
///
/// ```nocompile
/// match_and_insert!(
///     target = [{ Some content with { at some point match pattern } other match pattern are ignored }]
///     pattern = [{ match pattern }] // the match pattern cannot contain any group: `[]`, `()`, `{}`
/// 								  // can relax this constraint, but will require modifying the match logic in code
///     tokens = [{ expansion tokens }] // content inside braces can be anything including groups
/// );
/// ```
///
/// will generate:
///
/// ```nocompile
///     Some content with { at some point match pattern expansion tokens } other match patterns are
///     ignored
/// ```
#[proc_macro]
pub fn match_and_insert(input: TokenStream) -> TokenStream {
	match_and_insert::match_and_insert(input)
}

#[proc_macro_derive(PalletError, attributes(codec))]
pub fn derive_pallet_error(input: TokenStream) -> TokenStream {
	pallet_error::derive_pallet_error(input)
}

/// Internal macro used by `frame_support` to create tt-call-compliant macros
#[proc_macro]
pub fn __create_tt_macro(input: TokenStream) -> TokenStream {
	tt_macro::create_tt_return_macro(input)
}

#[proc_macro_attribute]
pub fn storage_alias(attributes: TokenStream, input: TokenStream) -> TokenStream {
	storage_alias::storage_alias(attributes.into(), input.into())
		.unwrap_or_else(|r| r.into_compile_error())
		.into()
}

/// This attribute can be used to derive a full implementation of a trait based on a local partial
/// impl and an external impl containing defaults that can be overriden in the local impl.
///
/// For a full end-to-end example, see [below](#use-case-auto-derive-test-pallet-config-traits).
///
/// # Usage
///
/// The attribute should be attached to an impl block (strictly speaking a `syn::ItemImpl`) for
/// which we want to inject defaults in the event of missing trait items in the block.
///
/// The attribute minimally takes a single `default_impl_path` argument, which should be the module
/// path to an impl registered via [`#[register_default_impl]`](`macro@register_default_impl`) that
/// contains the default trait items we want to potentially inject, with the general form:
///
/// ```ignore
/// #[derive_impl(default_impl_path)]
/// impl SomeTrait for SomeStruct {
///     ...
/// }
/// ```
///
/// Optionally, a `disambiguation_path` can be specified as follows by providing `as path::here`
/// after the `default_impl_path`:
///
/// ```ignore
/// #[derive_impl(default_impl_path as disambiguation_path)]
/// impl SomeTrait for SomeStruct {
///     ...
/// }
/// ```
///
/// The `disambiguation_path`, if specified, should be the path to a trait that will be used to
/// qualify all default entries that are injected into the local impl. For example if your
/// `default_impl_path` is `some::path::TestTraitImpl` and your `disambiguation_path` is
/// `another::path::DefaultTrait`, any items injected into the local impl will be qualified as
/// `<some::path::TestTraitImpl as another::path::DefaultTrait>::specific_trait_item`.
///
/// If you omit the `as disambiguation_path` portion, the `disambiguation_path` will internally
/// default to `A` from the `impl A for B` part of the default impl. This is useful for scenarios
/// where all of the relevant types are already in scope via `use` statements.
///
/// Conversely, the `default_impl_path` argument is required and cannot be omitted.
///
/// Optionally, `no_aggregated_types` can be specified as follows:
///
/// ```ignore
/// #[derive_impl(default_impl_path as disambiguation_path, no_aggregated_types)]
/// impl SomeTrait for SomeStruct {
///     ...
/// }
/// ```
///
/// If specified, this indicates that the aggregated types (as denoted by impl items
/// attached with [`#[inject_runtime_type]`]) should not be injected with the respective concrete
/// types. By default, all such types are injected.
///
/// You can also make use of `#[pallet::no_default]` on specific items in your default impl that you
/// want to ensure will not be copied over but that you nonetheless want to use locally in the
/// context of the foreign impl and the pallet (or context) in which it is defined.
///
/// ## Use-Case Example: Auto-Derive Test Pallet Config Traits
///
/// The `#[derive_imp(..)]` attribute can be used to derive a test pallet `Config` based on an
/// existing pallet `Config` that has been marked with
/// [`#[pallet::config(with_default)]`](`macro@config`) (which under the hood, generates a
/// `DefaultConfig` trait in the pallet in which the macro was invoked).
///
/// In this case, the `#[derive_impl(..)]` attribute should be attached to an `impl` block that
/// implements a compatible `Config` such as `frame_system::Config` for a test/mock runtime, and
/// should receive as its first argument the path to a `DefaultConfig` impl that has been registered
/// via [`#[register_default_impl]`](`macro@register_default_impl`), and as its second argument, the
/// path to the auto-generated `DefaultConfig` for the existing pallet `Config` we want to base our
/// test config off of.
///
/// The following is what the `basic` example pallet would look like with a default testing config:
///
/// ```ignore
/// #[derive_impl(frame_system::config_preludes::TestDefaultConfig as frame_system::pallet::DefaultConfig)]
/// impl frame_system::Config for Test {
///     // These are all defined by system as mandatory.
///     type BaseCallFilter = frame_support::traits::Everything;
///     type RuntimeEvent = RuntimeEvent;
///     type RuntimeCall = RuntimeCall;
///     type RuntimeOrigin = RuntimeOrigin;
///     type OnSetCode = ();
///     type PalletInfo = PalletInfo;
///     type Block = Block;
///     // We decide to override this one.
///     type AccountData = pallet_balances::AccountData<u64>;
/// }
/// ```
///
/// where `TestDefaultConfig` was defined and registered as follows:
///
/// ```ignore
/// pub struct TestDefaultConfig;
///
/// #[register_default_impl(TestDefaultConfig)]
/// impl DefaultConfig for TestDefaultConfig {
///     type Version = ();
///     type BlockWeights = ();
///     type BlockLength = ();
///     type DbWeight = ();
///     type Nonce = u64;
///     type BlockNumber = u64;
///     type Hash = sp_core::hash::H256;
///     type Hashing = sp_runtime::traits::BlakeTwo256;
///     type AccountId = AccountId;
///     type Lookup = IdentityLookup<AccountId>;
///     type BlockHashCount = frame_support::traits::ConstU64<10>;
///     type AccountData = u32;
///     type OnNewAccount = ();
///     type OnKilledAccount = ();
///     type SystemWeightInfo = ();
///     type SS58Prefix = ();
///     type MaxConsumers = frame_support::traits::ConstU32<16>;
/// }
/// ```
///
/// The above call to `derive_impl` would expand to roughly the following:
///
/// ```ignore
/// impl frame_system::Config for Test {
///     use frame_system::config_preludes::TestDefaultConfig;
///     use frame_system::pallet::DefaultConfig;
///
///     type BaseCallFilter = frame_support::traits::Everything;
///     type RuntimeEvent = RuntimeEvent;
///     type RuntimeCall = RuntimeCall;
///     type RuntimeOrigin = RuntimeOrigin;
///     type OnSetCode = ();
///     type PalletInfo = PalletInfo;
///     type Block = Block;
///     type AccountData = pallet_balances::AccountData<u64>;
///     type Version = <TestDefaultConfig as DefaultConfig>::Version;
///     type BlockWeights = <TestDefaultConfig as DefaultConfig>::BlockWeights;
///     type BlockLength = <TestDefaultConfig as DefaultConfig>::BlockLength;
///     type DbWeight = <TestDefaultConfig as DefaultConfig>::DbWeight;
///     type Nonce = <TestDefaultConfig as DefaultConfig>::Nonce;
///     type BlockNumber = <TestDefaultConfig as DefaultConfig>::BlockNumber;
///     type Hash = <TestDefaultConfig as DefaultConfig>::Hash;
///     type Hashing = <TestDefaultConfig as DefaultConfig>::Hashing;
///     type AccountId = <TestDefaultConfig as DefaultConfig>::AccountId;
///     type Lookup = <TestDefaultConfig as DefaultConfig>::Lookup;
///     type BlockHashCount = <TestDefaultConfig as DefaultConfig>::BlockHashCount;
///     type OnNewAccount = <TestDefaultConfig as DefaultConfig>::OnNewAccount;
///     type OnKilledAccount = <TestDefaultConfig as DefaultConfig>::OnKilledAccount;
///     type SystemWeightInfo = <TestDefaultConfig as DefaultConfig>::SystemWeightInfo;
///     type SS58Prefix = <TestDefaultConfig as DefaultConfig>::SS58Prefix;
///     type MaxConsumers = <TestDefaultConfig as DefaultConfig>::MaxConsumers;
/// }
/// ```
///
/// You can then use the resulting `Test` config in test scenarios.
///
/// Note that items that are _not_ present in our local `DefaultConfig` are automatically copied
/// from the foreign trait (in this case `TestDefaultConfig`) into the local trait impl (in this
/// case `Test`), unless the trait item in the local trait impl is marked with
/// [`#[pallet::no_default]`](`macro@no_default`), in which case it cannot be overridden, and any
/// attempts to do so will result in a compiler error.
///
/// See `frame/examples/default-config/tests.rs` for a runnable end-to-end example pallet that makes
/// use of `derive_impl` to derive its testing config.
///
/// See [here](`macro@config`) for more information and caveats about the auto-generated
/// `DefaultConfig` trait.
///
/// ## Optional Conventions
///
/// Note that as an optional convention, we encourage creating a `config_preludes` module inside of
/// your pallet. This is the convention we follow for `frame_system`'s `TestDefaultConfig` which, as
/// shown above, is located at `frame_system::config_preludes::TestDefaultConfig`. This is just a
/// suggested convention -- there is nothing in the code that expects modules with these names to be
/// in place, so there is no imperative to follow this pattern unless desired.
///
/// In `config_preludes`, you can place types named like:
///
/// * `TestDefaultConfig`
/// * `ParachainDefaultConfig`
/// * `SolochainDefaultConfig`
///
/// Signifying in which context they can be used.
///
/// # Advanced Usage
///
/// ## Expansion
///
/// The `#[derive_impl(default_impl_path as disambiguation_path)]` attribute will expand to the
/// local impl, with any extra items from the foreign impl that aren't present in the local impl
/// also included. In the case of a colliding trait item, the version of the item that exists in the
/// local impl will be retained. All imported items are qualified by the `disambiguation_path`, as
/// discussed above.
///
/// ## Handling of Unnamed Trait Items
///
/// Items that lack a `syn::Ident` for whatever reason are first checked to see if they exist,
/// verbatim, in the local/destination trait before they are copied over, so you should not need to
/// worry about collisions between identical unnamed items.
#[import_tokens_attr_verbatim {
    format!(
        "{}::macro_magic",
        match generate_access_from_frame_or_crate("frame-support") {
            Ok(path) => Ok(path),
            Err(_) => generate_access_from_frame_or_crate("frame"),
        }
        .expect("Failed to find either `frame-support` or `frame` in `Cargo.toml` dependencies.")
        .to_token_stream()
        .to_string()
    )
}]
#[with_custom_parsing(derive_impl::DeriveImplAttrArgs)]
#[proc_macro_attribute]
pub fn derive_impl(attrs: TokenStream, input: TokenStream) -> TokenStream {
	let custom_attrs = parse_macro_input!(__custom_tokens as derive_impl::DeriveImplAttrArgs);
	derive_impl::derive_impl(
		__source_path.into(),
		attrs.into(),
		input.into(),
		custom_attrs.disambiguation_path,
		custom_attrs.no_aggregated_types,
	)
	.unwrap_or_else(|r| r.into_compile_error())
	.into()
}

/// The optional attribute `#[pallet::no_default]` can be attached to trait items within a
/// `Config` trait impl that has [`#[pallet::config(with_default)]`](`macro@config`) attached.
///
/// Attaching this attribute to a trait item ensures that that trait item will not be used as a
/// default with the [`#[derive_impl(..)]`](`macro@derive_impl`) attribute macro.
#[proc_macro_attribute]
pub fn no_default(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The optional attribute `#[pallet::no_default_bounds]` can be attached to trait items within a
/// `Config` trait impl that has [`#[pallet::config(with_default)]`](`macro@config`) attached.
///
/// Attaching this attribute to a trait item ensures that the generated trait `DefaultConfig`
/// will not have any bounds for this trait item.
///
/// As an example, if you have a trait item `type AccountId: SomeTrait;` in your `Config` trait,
/// the generated `DefaultConfig` will only have `type AccountId;` with no trait bound.
#[proc_macro_attribute]
pub fn no_default_bounds(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Attach this attribute to an impl statement that you want to use with
/// [`#[derive_impl(..)]`](`macro@derive_impl`).
///
/// You must also provide an identifier/name as the attribute's argument. This is the name you
/// must provide to [`#[derive_impl(..)]`](`macro@derive_impl`) when you import this impl via
/// the `default_impl_path` argument. This name should be unique at the crate-level.
///
/// ## Example
///
/// ```ignore
/// pub struct ExampleTestDefaultConfig;
///
/// #[register_default_impl(ExampleTestDefaultConfig)]
/// impl DefaultConfig for ExampleTestDefaultConfig {
/// 	type Version = ();
/// 	type BlockWeights = ();
/// 	type BlockLength = ();
/// 	...
/// 	type SS58Prefix = ();
/// 	type MaxConsumers = frame_support::traits::ConstU32<16>;
/// }
/// ```
///
/// ## Advanced Usage
///
/// This macro acts as a thin wrapper around macro_magic's `#[export_tokens]`. See the docs
/// [here](https://docs.rs/macro_magic/latest/macro_magic/attr.export_tokens.html) for more
/// info.
///
/// There are some caveats when applying a `use` statement to bring a
/// `#[register_default_impl]` item into scope. If you have a `#[register_default_impl]`
/// defined in `my_crate::submodule::MyItem`, it is currently not sufficient to do something
/// like:
///
/// ```ignore
/// use my_crate::submodule::MyItem;
/// #[derive_impl(MyItem as Whatever)]
/// ```
///
/// This will fail with a mysterious message about `__export_tokens_tt_my_item` not being
/// defined.
///
/// You can, however, do any of the following:
/// ```ignore
/// // partial path works
/// use my_crate::submodule;
/// #[derive_impl(submodule::MyItem as Whatever)]
/// ```
/// ```ignore
/// // full path works
/// #[derive_impl(my_crate::submodule::MyItem as Whatever)]
/// ```
/// ```ignore
/// // wild-cards work
/// use my_crate::submodule::*;
/// #[derive_impl(MyItem as Whatever)]
/// ```
#[proc_macro_attribute]
pub fn register_default_impl(attrs: TokenStream, tokens: TokenStream) -> TokenStream {
	// ensure this is a impl statement
	let item_impl = syn::parse_macro_input!(tokens as ItemImpl);

	// internally wrap macro_magic's `#[export_tokens]` macro
	match macro_magic::mm_core::export_tokens_internal(
		attrs,
		item_impl.to_token_stream(),
		true,
		false,
	) {
		Ok(tokens) => tokens.into(),
		Err(err) => err.to_compile_error().into(),
	}
}

#[proc_macro_attribute]
pub fn inject_runtime_type(_: TokenStream, tokens: TokenStream) -> TokenStream {
	let item = tokens.clone();
	let item = syn::parse_macro_input!(item as TraitItemType);
	if item.ident != "RuntimeCall" &&
		item.ident != "RuntimeEvent" &&
		item.ident != "RuntimeOrigin" &&
		item.ident != "RuntimeHoldReason" &&
		item.ident != "RuntimeFreezeReason" &&
		item.ident != "PalletInfo"
	{
		return syn::Error::new_spanned(
			item,
			"`#[inject_runtime_type]` can only be attached to `RuntimeCall`, `RuntimeEvent`, `RuntimeOrigin` or `PalletInfo`",
		)
		.to_compile_error()
		.into();
	}
	tokens
}

/// Used internally to decorate pallet attribute macro stubs when they are erroneously used
/// outside of a pallet module
fn pallet_macro_stub() -> TokenStream {
	quote!(compile_error!(
		"This attribute can only be used from within a pallet module marked with `#[frame_support::pallet]`"
	))
	.into()
}

/// The mandatory attribute `#[pallet::config]` defines the configurable options for the pallet.
///
/// Item must be defined as:
///
/// ```ignore
/// #[pallet::config]
/// pub trait Config: frame_system::Config + $optionally_some_other_supertraits
/// $optional_where_clause
/// {
/// ...
/// }
/// ```
///
/// I.e. a regular trait definition named `Config`, with the supertrait
/// `frame_system::pallet::Config`, and optionally other supertraits and a where clause.
/// (Specifying other supertraits here is known as [tight
/// coupling](https://docs.substrate.io/reference/how-to-guides/pallet-design/use-tight-coupling/))
///
/// The associated type `RuntimeEvent` is reserved. If defined, it must have the bounds
/// `From<Event>` and `IsType<<Self as frame_system::Config>::RuntimeEvent>`.
///
/// [`pallet::event`](`macro@event`) must be present if `RuntimeEvent` exists as a config item
/// in your `#[pallet::config]`.
///
/// ## Optional: `with_default`
///
/// An optional `with_default` argument may also be specified. Doing so will automatically
/// generate a `DefaultConfig` trait inside your pallet which is suitable for use with
/// [`[#[derive_impl(..)]`](`macro@derive_impl`) to derive a default testing config:
///
/// ```ignore
/// #[pallet::config(with_default)]
/// pub trait Config: frame_system::Config {
/// 		type RuntimeEvent: Parameter
/// 			+ Member
/// 			+ From<Event<Self>>
/// 			+ Debug
/// 			+ IsType<<Self as frame_system::Config>::RuntimeEvent>;
///
/// 		#[pallet::no_default]
/// 		type BaseCallFilter: Contains<Self::RuntimeCall>;
/// 	// ...
/// }
/// ```
///
/// As shown above, you may also attach the [`#[pallet::no_default]`](`macro@no_default`)
/// attribute to specify that a particular trait item _cannot_ be used as a default when a test
/// `Config` is derived using the [`#[derive_impl(..)]`](`macro@derive_impl`) attribute macro.
/// This will cause that particular trait item to simply not appear in default testing configs
/// based on this config (the trait item will not be included in `DefaultConfig`).
///
/// ### `DefaultConfig` Caveats
///
/// The auto-generated `DefaultConfig` trait:
/// - is always a _subset_ of your pallet's `Config` trait.
/// - can only contain items that don't rely on externalities, such as `frame_system::Config`.
///
/// Trait items that _do_ rely on externalities should be marked with
/// [`#[pallet::no_default]`](`macro@no_default`)
///
/// Consequently:
/// - Any items that rely on externalities _must_ be marked with
///   [`#[pallet::no_default]`](`macro@no_default`) or your trait will fail to compile when used
///   with [`derive_impl`](`macro@derive_impl`).
/// - Items marked with [`#[pallet::no_default]`](`macro@no_default`) are entirely excluded from the
///   `DefaultConfig` trait, and therefore any impl of `DefaultConfig` doesn't need to implement
///   such items.
///
/// For more information, see [`macro@derive_impl`].
#[proc_macro_attribute]
pub fn config(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

///
/// ---
///
/// **Rust-Analyzer users**: See the documentation of the Rust item in
/// `frame_support::pallet_macros::constant`.
#[proc_macro_attribute]
pub fn constant(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

///
/// ---
///
/// **Rust-Analyzer users**: See the documentation of the Rust item in
/// `frame_support::pallet_macros::constant_name`.
#[proc_macro_attribute]
pub fn constant_name(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// To bypass the `frame_system::Config` supertrait check, use the attribute
/// `pallet::disable_frame_system_supertrait_check`, e.g.:
///
/// ```ignore
/// #[pallet::config]
/// #[pallet::disable_frame_system_supertrait_check]
/// pub trait Config: pallet_timestamp::Config {}
/// ```
///
/// NOTE: Bypassing the `frame_system::Config` supertrait check is typically desirable when you
/// want to write an alternative to the `frame_system` pallet.
#[proc_macro_attribute]
pub fn disable_frame_system_supertrait_check(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// To generate a `Store` trait associating all storages, annotate your `Pallet` struct with
/// the attribute `#[pallet::generate_store($vis trait Store)]`, e.g.:
///
/// ```ignore
/// #[pallet::pallet]
/// #[pallet::generate_store(pub(super) trait Store)]
/// pub struct Pallet<T>(_);
/// ```
/// More precisely, the `Store` trait contains an associated type for each storage. It is
/// implemented for `Pallet` allowing access to the storage from pallet struct.
///
/// Thus when defining a storage named `Foo`, it can later be accessed from `Pallet` using
/// `<Pallet as Store>::Foo`.
///
/// NOTE: this attribute is only valid when applied _directly_ to your `Pallet` struct
/// definition.
#[proc_macro_attribute]
pub fn generate_store(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Because the `pallet::pallet` macro implements `GetStorageVersion`, the current storage
/// version needs to be communicated to the macro. This can be done by using the
/// `pallet::storage_version` attribute:
///
/// ```ignore
/// const STORAGE_VERSION: StorageVersion = StorageVersion::new(5);
///
/// #[pallet::pallet]
/// #[pallet::storage_version(STORAGE_VERSION)]
/// pub struct Pallet<T>(_);
/// ```
///
/// If not present, the current storage version is set to the default value.
#[proc_macro_attribute]
pub fn storage_version(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The `#[pallet::hooks]` attribute allows you to specify a `Hooks` implementation for
/// `Pallet` that specifies pallet-specific logic.
///
/// The item the attribute attaches to must be defined as follows:
/// ```ignore
/// #[pallet::hooks]
/// impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> $optional_where_clause {
///     ...
/// }
/// ```
/// I.e. a regular trait implementation with generic bound: `T: Config`, for the trait
/// `Hooks<BlockNumberFor<T>>` (they are defined in preludes), for the type `Pallet<T>` and
/// with an optional where clause.
///
/// If no `#[pallet::hooks]` exists, then the following default implementation is
/// automatically generated:
/// ```ignore
/// #[pallet::hooks]
/// impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}
/// ```
///
/// ## Macro expansion
///
/// The macro implements the traits `OnInitialize`, `OnIdle`, `OnFinalize`, `OnRuntimeUpgrade`,
/// `OffchainWorker`, and `IntegrityTest` using the provided `Hooks` implementation.
///
/// NOTE: `OnRuntimeUpgrade` is implemented with `Hooks::on_runtime_upgrade` and some
/// additional logic. E.g. logic to write the pallet version into storage.
///
/// NOTE: The macro also adds some tracing logic when implementing the above traits. The
/// following hooks emit traces: `on_initialize`, `on_finalize` and `on_runtime_upgrade`.
#[proc_macro_attribute]
pub fn hooks(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Each dispatchable needs to define a weight with `#[pallet::weight($expr)]` attribute, the
/// first argument must be `origin: OriginFor<T>`.
#[proc_macro_attribute]
pub fn weight(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Compact encoding for arguments can be achieved via `#[pallet::compact]`. The function must
/// return a `DispatchResultWithPostInfo` or `DispatchResult`.
#[proc_macro_attribute]
pub fn compact(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

///
/// ---
///
/// **Rust-Analyzer users**: See the documentation of the Rust item in
/// `frame_support::pallet_macros::call`.
#[proc_macro_attribute]
pub fn call(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Each dispatchable may also be annotated with the `#[pallet::call_index($idx)]` attribute,
/// which explicitly defines the codec index for the dispatchable function in the `Call` enum.
///
/// All call indexes start from 0, until it encounters a dispatchable function with a defined
/// call index. The dispatchable function that lexically follows the function with a defined
/// call index will have that call index, but incremented by 1, e.g. if there are 3
/// dispatchable functions `fn foo`, `fn bar` and `fn qux` in that order, and only `fn bar`
/// has a call index of 10, then `fn qux` will have an index of 11, instead of 1.
///
/// All arguments must implement [`Debug`], [`PartialEq`], [`Eq`], `Decode`, `Encode`, and
/// [`Clone`]. For ease of use, bound by the trait `frame_support::pallet_prelude::Member`.
///
/// If no `#[pallet::call]` exists, then a default implementation corresponding to the
/// following code is automatically generated:
///
/// ```ignore
/// #[pallet::call]
/// impl<T: Config> Pallet<T> {}
/// ```
///
/// **WARNING**: modifying dispatchables, changing their order, removing some, etc., must be
/// done with care. Indeed this will change the outer runtime call type (which is an enum with
/// one variant per pallet), this outer runtime call can be stored on-chain (e.g. in
/// `pallet-scheduler`). Thus migration might be needed. To mitigate against some of this, the
/// `#[pallet::call_index($idx)]` attribute can be used to fix the order of the dispatchable so
/// that the `Call` enum encoding does not change after modification. As a general rule of
/// thumb, it is therefore adventageous to always add new calls to the end so you can maintain
/// the existing order of calls.
///
/// ### Macro expansion
///
/// The macro creates an enum `Call` with one variant per dispatchable. This enum implements:
/// [`Clone`], [`Eq`], [`PartialEq`], [`Debug`] (with stripped implementation in `not("std")`),
/// `Encode`, `Decode`, `GetDispatchInfo`, `GetCallName`, `GetCallIndex` and
/// `UnfilteredDispatchable`.
///
/// The macro implements the `Callable` trait on `Pallet` and a function `call_functions`
/// which returns the dispatchable metadata.
#[proc_macro_attribute]
pub fn call_index(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Each dispatchable may be annotated with the `#[pallet::feeless_if($closure)]` attribute,
/// which explicitly defines the condition for the dispatchable to be feeless.
///
/// The arguments for the closure must be the referenced arguments of the dispatchable function.
///
/// The closure must return `bool`.
///
/// ### Example
/// ```ignore
/// #[pallet::feeless_if(|_origin: &OriginFor<T>, something: &u32| -> bool {
/// 		*something == 0
/// 	})]
/// pub fn do_something(origin: OriginFor<T>, something: u32) -> DispatchResult {
///     ....
/// }
/// ```
///
/// Please note that this only works for signed dispatchables and requires a signed extension
/// such as `SkipCheckIfFeeless` as defined in `pallet-skip-feeless-payment` to wrap the existing
/// payment extension. Else, this is completely ignored and the dispatchable is still charged.
///
/// ### Macro expansion
///
/// The macro implements the `CheckIfFeeless` trait on the dispatchable and calls the corresponding
/// closure in the implementation.
#[proc_macro_attribute]
pub fn feeless_if(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Allows you to define some extra constants to be added into constant metadata.
///
/// Item must be defined as:
///
/// ```ignore
/// #[pallet::extra_constants]
/// impl<T: Config> Pallet<T> where $optional_where_clause {
/// 	/// $some_doc
/// 	$vis fn $fn_name() -> $some_return_type {
/// 		...
/// 	}
/// 	...
/// }
/// ```
/// I.e. a regular rust `impl` block with some optional where clause and functions with 0 args,
/// 0 generics, and some return type.
///
/// ## Macro expansion
///
/// The macro add some extra constants to pallet constant metadata.
#[proc_macro_attribute]
pub fn extra_constants(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The `#[pallet::error]` attribute allows you to define an error enum that will be returned
/// from the dispatchable when an error occurs. The information for this error type is then
/// stored in metadata.
///
/// Item must be defined as:
///
/// ```ignore
/// #[pallet::error]
/// pub enum Error<T> {
/// 	/// $some_optional_doc
/// 	$SomeFieldLessVariant,
/// 	/// $some_more_optional_doc
/// 	$SomeVariantWithOneField(FieldType),
/// 	...
/// }
/// ```
/// I.e. a regular enum named `Error`, with generic `T` and fieldless or multiple-field
/// variants.
///
/// Any field type in the enum variants must implement `TypeInfo` in order to be properly used
/// in the metadata, and its encoded size should be as small as possible, preferably 1 byte in
/// size in order to reduce storage size. The error enum itself has an absolute maximum encoded
/// size specified by `MAX_MODULE_ERROR_ENCODED_SIZE`.
///
/// (1 byte can still be 256 different errors. The more specific the error, the easier it is to
/// diagnose problems and give a better experience to the user. Don't skimp on having lots of
/// individual error conditions.)
///
/// Field types in enum variants must also implement `PalletError`, otherwise the pallet will
/// fail to compile. Rust primitive types have already implemented the `PalletError` trait
/// along with some commonly used stdlib types such as [`Option`] and `PhantomData`, and hence
/// in most use cases, a manual implementation is not necessary and is discouraged.
///
/// The generic `T` must not bound anything and a `where` clause is not allowed. That said,
/// bounds and/or a where clause should not needed for any use-case.
///
/// ## Macro expansion
///
/// The macro implements the [`Debug`] trait and functions `as_u8` using variant position, and
/// `as_str` using variant doc.
///
/// The macro also implements `From<Error<T>>` for `&'static str` and `From<Error<T>>` for
/// `DispatchError`.
#[proc_macro_attribute]
pub fn error(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The `#[pallet::event]` attribute allows you to define pallet events. Pallet events are
/// stored under the `system` / `events` key when the block is applied (and then replaced when
/// the next block writes it's events).
///
/// The Event enum must be defined as follows:
///
/// ```ignore
/// #[pallet::event]
/// #[pallet::generate_deposit($visibility fn deposit_event)] // Optional
/// pub enum Event<$some_generic> $optional_where_clause {
/// 	/// Some doc
/// 	$SomeName($SomeType, $YetanotherType, ...),
/// 	...
/// }
/// ```
///
/// I.e. an enum (with named or unnamed fields variant), named `Event`, with generic: none or
/// `T` or `T: Config`, and optional w here clause.
///
/// Each field must implement [`Clone`], [`Eq`], [`PartialEq`], `Encode`, `Decode`, and
/// [`Debug`] (on std only). For ease of use, bound by the trait `Member`, available in
/// `frame_support::pallet_prelude`.
#[proc_macro_attribute]
pub fn event(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The attribute `#[pallet::generate_deposit($visibility fn deposit_event)]` generates a
/// helper function on `Pallet` that handles deposit events.
///
/// NOTE: For instantiable pallets, the event must be generic over `T` and `I`.
///
/// ## Macro expansion
///
/// The macro will add on enum `Event` the attributes:
/// * `#[derive(frame_support::CloneNoBound)]`
/// * `#[derive(frame_support::EqNoBound)]`
/// * `#[derive(frame_support::PartialEqNoBound)]`
/// * `#[derive(frame_support::RuntimeDebugNoBound)]`
/// * `#[derive(codec::Encode)]`
/// * `#[derive(codec::Decode)]`
///
/// The macro implements `From<Event<..>>` for ().
///
/// The macro implements a metadata function on `Event` returning the `EventMetadata`.
///
/// If `#[pallet::generate_deposit]` is present then the macro implements `fn deposit_event` on
/// `Pallet`.
#[proc_macro_attribute]
pub fn generate_deposit(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

///
/// ---
///
/// **Rust-Analyzer users**: See the documentation of the Rust item in
/// `frame_support::pallet_macros::storage`.
#[proc_macro_attribute]
pub fn storage(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The optional attribute `#[pallet::getter(fn $my_getter_fn_name)]` allows you to define a
/// getter function on `Pallet`.
///
/// Also see [`pallet::storage`](`macro@storage`)
#[proc_macro_attribute]
pub fn getter(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The optional attribute `#[pallet::storage_prefix = "SomeName"]` allows you to define the
/// storage prefix to use. This is helpful if you wish to rename the storage field but don't
/// want to perform a migration.
///
/// E.g:
///
/// ```ignore
/// #[pallet::storage]
/// #[pallet::storage_prefix = "foo"]
/// #[pallet::getter(fn my_storage)]
/// pub(super) type MyStorage<T> = StorageMap<Hasher = Blake2_128Concat, Key = u32, Value = u32>;
/// ```
///
/// or
///
/// ```ignore
/// #[pallet::storage]
/// #[pallet::getter(fn my_storage)]
/// pub(super) type MyStorage<T> = StorageMap<_, Blake2_128Concat, u32, u32>;
/// ```
#[proc_macro_attribute]
pub fn storage_prefix(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The optional attribute `#[pallet::unbounded]` declares the storage as unbounded. When
/// implementating the storage info (when `#[pallet::generate_storage_info]` is specified on
/// the pallet struct placeholder), the size of the storage will be declared as unbounded. This
/// can be useful for storage which can never go into PoV (Proof of Validity).
#[proc_macro_attribute]
pub fn unbounded(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The optional attribute `#[pallet::whitelist_storage]` will declare the
/// storage as whitelisted from benchmarking. Doing so will exclude reads of
/// that value's storage key from counting towards weight calculations during
/// benchmarking.
///
/// This attribute should only be attached to storages that are known to be
/// read/used in every block. This will result in a more accurate benchmarking weight.
///
/// ### Example
/// ```ignore
/// #[pallet::storage]
/// #[pallet::whitelist_storage]
/// pub(super) type Number<T: Config> = StorageValue<_, frame_system::pallet_prelude::BlockNumberFor::<T>, ValueQuery>;
/// ```
///
/// NOTE: As with all `pallet::*` attributes, this one _must_ be written as
/// `#[pallet::whitelist_storage]` and can only be placed inside a `pallet` module in order for
/// it to work properly.
#[proc_macro_attribute]
pub fn whitelist_storage(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The `#[pallet::type_value]` attribute lets you define a struct implementing the `Get` trait
/// to ease the use of storage types. This attribute is meant to be used alongside
/// [`#[pallet::storage]`](`macro@storage`) to define a storage's default value. This attribute
/// can be used multiple times.
///
/// Item must be defined as:
///
/// ```ignore
/// #[pallet::type_value]
/// fn $MyDefaultName<$some_generic>() -> $default_type $optional_where_clause { $expr }
/// ```
///
/// I.e.: a function definition with generics none or `T: Config` and a returned type.
///
/// E.g.:
///
/// ```ignore
/// #[pallet::type_value]
/// fn MyDefault<T: Config>() -> T::Balance { 3.into() }
/// ```
///
/// ## Macro expansion
///
/// The macro renames the function to some internal name, generates a struct with the original
/// name of the function and its generic, and implements `Get<$ReturnType>` by calling the user
/// defined function.
#[proc_macro_attribute]
pub fn type_value(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

///
/// ---
///
/// **Rust-Analyzer users**: See the documentation of the Rust item in
/// `frame_support::pallet_macros::genesis_config`.
#[proc_macro_attribute]
pub fn genesis_config(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

///
/// ---
///
/// **Rust-Analyzer users**: See the documentation of the Rust item in
/// `frame_support::pallet_macros::genesis_build`.
#[proc_macro_attribute]
pub fn genesis_build(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The `#[pallet::inherent]` attribute allows the pallet to provide some
/// [inherent](https://docs.substrate.io/fundamentals/transaction-types/#inherent-transactions).
/// An inherent is some piece of data that is inserted by a block authoring node at block
/// creation time and can either be accepted or rejected by validators based on whether the
/// data falls within an acceptable range.
///
/// The most common inherent is the `timestamp` that is inserted into every block. Since there
/// is no way to validate timestamps, validators simply check that the timestamp reported by
/// the block authoring node falls within an acceptable range.
///
/// Item must be defined as:
///
/// ```ignore
/// #[pallet::inherent]
/// impl<T: Config> ProvideInherent for Pallet<T> {
/// 	// ... regular trait implementation
/// }
/// ```
///
/// I.e. a trait implementation with bound `T: Config`, of trait `ProvideInherent` for type
/// `Pallet<T>`, and some optional where clause.
///
/// ## Macro expansion
///
/// The macro currently makes no use of this information, but it might use this information in
/// the future to give information directly to `construct_runtime`.
#[proc_macro_attribute]
pub fn inherent(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The `#[pallet::validate_unsigned]` attribute allows the pallet to validate some unsigned
/// transaction:
///
/// Item must be defined as:
///
/// ```ignore
/// #[pallet::validate_unsigned]
/// impl<T: Config> ValidateUnsigned for Pallet<T> {
/// 	// ... regular trait implementation
/// }
/// ```
///
/// I.e. a trait implementation with bound `T: Config`, of trait `ValidateUnsigned` for type
/// `Pallet<T>`, and some optional where clause.
///
/// NOTE: There is also the `sp_runtime::traits::SignedExtension` trait that can be used to add
/// some specific logic for transaction validation.
///
/// ## Macro expansion
///
/// The macro currently makes no use of this information, but it might use this information in
/// the future to give information directly to `construct_runtime`.
#[proc_macro_attribute]
pub fn validate_unsigned(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The `#[pallet::origin]` attribute allows you to define some origin for the pallet.
///
/// Item must be either a type alias, an enum, or a struct. It needs to be public.
///
/// E.g.:
///
/// ```ignore
/// #[pallet::origin]
/// pub struct Origin<T>(PhantomData<(T)>);
/// ```
///
/// **WARNING**: modifying origin changes the outer runtime origin. This outer runtime origin
/// can be stored on-chain (e.g. in `pallet-scheduler`), thus any change must be done with care
/// as it might require some migration.
///
/// NOTE: for instantiable pallets, the origin must be generic over `T` and `I`.
#[proc_macro_attribute]
pub fn origin(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// The `#[pallet::composite_enum]` attribute allows you to define an enum that gets composed as an
/// aggregate enum by `construct_runtime`. This is similar in principle with `#[pallet::event]` and
/// `#[pallet::error]`.
///
/// The attribute currently only supports enum definitions, and identifiers that are named
/// `FreezeReason`, `HoldReason`, `LockId` or `SlashReason`. Arbitrary identifiers for the enum are
/// not supported. The aggregate enum generated by `construct_runtime` will have the name of
/// `RuntimeFreezeReason`, `RuntimeHoldReason`, `RuntimeLockId` and `RuntimeSlashReason`
/// respectively.
///
/// NOTE: The aggregate enum generated by `construct_runtime` generates a conversion function from
/// the pallet enum to the aggregate enum, and automatically derives the following traits:
///
/// ```ignore
/// Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Encode, Decode, MaxEncodedLen, TypeInfo,
/// RuntimeDebug
/// ```
///
/// For ease of usage, when no `#[derive]` attributes are found for the enum under
/// `#[pallet::composite_enum]`, the aforementioned traits are automatically derived for it. The
/// inverse is also true: if there are any `#[derive]` attributes found for the enum, then no traits
/// will automatically be derived for it.
#[proc_macro_attribute]
pub fn composite_enum(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Can be attached to a module. Doing so will declare that module as importable into a pallet
/// via [`#[import_section]`](`macro@import_section`).
///
/// Note that sections are imported by their module name/ident, and should be referred to by
/// their _full path_ from the perspective of the target pallet. Do not attempt to make use
/// of `use` statements to bring pallet sections into scope, as this will not work (unless
/// you do so as part of a wildcard import, in which case it will work).
///
/// ## Naming Logistics
///
/// Also note that because of how `#[pallet_section]` works, pallet section names must be
/// globally unique _within the crate in which they are defined_. For more information on
/// why this must be the case, see macro_magic's
/// [`#[export_tokens]`](https://docs.rs/macro_magic/latest/macro_magic/attr.export_tokens.html) macro.
///
/// Optionally, you may provide an argument to `#[pallet_section]` such as
/// `#[pallet_section(some_ident)]`, in the event that there is another pallet section in
/// same crate with the same ident/name. The ident you specify can then be used instead of
/// the module's ident name when you go to import it via `#[import_section]`.
#[proc_macro_attribute]
pub fn pallet_section(attr: TokenStream, tokens: TokenStream) -> TokenStream {
	let tokens_clone = tokens.clone();
	// ensure this can only be attached to a module
	let _mod = parse_macro_input!(tokens_clone as ItemMod);

	// use macro_magic's export_tokens as the internal implementation otherwise
	match macro_magic::mm_core::export_tokens_internal(attr, tokens, false, true) {
		Ok(tokens) => tokens.into(),
		Err(err) => err.to_compile_error().into(),
	}
}

/// An attribute macro that can be attached to a module declaration. Doing so will
/// Imports the contents of the specified external pallet section that was defined
/// previously using [`#[pallet_section]`](`macro@pallet_section`).
///
/// ## Example
/// ```ignore
/// #[import_section(some_section)]
/// #[pallet]
/// pub mod pallet {
///     // ...
/// }
/// ```
/// where `some_section` was defined elsewhere via:
/// ```ignore
/// #[pallet_section]
/// pub mod some_section {
///     // ...
/// }
/// ```
///
/// This will result in the contents of `some_section` being _verbatim_ imported into
/// the pallet above. Note that since the tokens for `some_section` are essentially
/// copy-pasted into the target pallet, you cannot refer to imports that don't also
/// exist in the target pallet, but this is easily resolved by including all relevant
/// `use` statements within your pallet section, so they are imported as well, or by
/// otherwise ensuring that you have the same imports on the target pallet.
///
/// It is perfectly permissible to import multiple pallet sections into the same pallet,
/// which can be done by having multiple `#[import_section(something)]` attributes
/// attached to the pallet.
///
/// Note that sections are imported by their module name/ident, and should be referred to by
/// their _full path_ from the perspective of the target pallet.
#[import_tokens_attr {
    format!(
        "{}::macro_magic",
        match generate_access_from_frame_or_crate("frame-support") {
            Ok(path) => Ok(path),
            Err(_) => generate_access_from_frame_or_crate("frame"),
        }
        .expect("Failed to find either `frame-support` or `frame` in `Cargo.toml` dependencies.")
        .to_token_stream()
        .to_string()
    )
}]
#[proc_macro_attribute]
pub fn import_section(attr: TokenStream, tokens: TokenStream) -> TokenStream {
	let foreign_mod = parse_macro_input!(attr as ItemMod);
	let mut internal_mod = parse_macro_input!(tokens as ItemMod);

	// check that internal_mod is a pallet module
	if !internal_mod.attrs.iter().any(|attr| {
		if let Some(last_seg) = attr.path().segments.last() {
			last_seg.ident == "pallet"
		} else {
			false
		}
	}) {
		return Error::new(
			internal_mod.ident.span(),
			"`#[import_section]` can only be applied to a valid pallet module",
		)
		.to_compile_error()
		.into()
	}

	if let Some(ref mut content) = internal_mod.content {
		if let Some(foreign_content) = foreign_mod.content {
			content.1.extend(foreign_content.1);
		}
	}

	quote! {
		#internal_mod
	}
	.into()
}
