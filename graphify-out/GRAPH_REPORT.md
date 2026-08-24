# Graph Report - tropel  (2026-08-25)

## Corpus Check
- 246 files · ~440,416 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 4405 nodes · 10365 edges · 204 communities (176 shown, 28 thin omitted)
- Extraction: 98% EXTRACTED · 2% INFERRED · 0% AMBIGUOUS · INFERRED: 160 edges (avg confidence: 0.84)
- Token cost: 0 input · 0 output

## Community Hubs (Navigation)
- HTTP Client & DNS Cache
- K6 HTTP Bridge & Driver
- OAuth2 Authorization Flow
- JS Context & Console Shims
- Engine Extension Registry
- HTTP Body & Headers Types
- Prometheus Output
- OpenAPI Spec Normalization
- JS Transpiler & Virtual Specifiers
- Metrics Aggregation Collector
- Threshold Evaluation Engine
- Dynamic Variable Catalog
- Browser WASI Shim (core-wasm)
- Cargo Build & Extension Deps
- WebAssembly Host Context
- gRPC Codec & Bounded Cache
- HTTP Digest Authentication
- WebSocket Integration Tests
- JSON Path & Escape Resolver
- HAR Input Adapter
- K6 Options & Threshold Conversion
- Runtime Smoke & JWT Claims
- VU Control Spawn Guards
- Crypto Module (AES/HMAC)
- Browser WASI Shim Package
- Metric Registry & Units
- Postman Collection Parser
- input-wasm npm Package
- HTTP File Input Adapter
- Reqwest Client & Redirects
- Bruno Input Adapter
- InfluxDB Output
- Postman Script Runner
- core-wasm npm Package
- gRPC Streaming Integration
- CSV Reporter
- Worker Join & Busy Guards
- VU Loop Sampler Tasks
- Execution Config & Arrival Rate
- Latency Histogram
- Live State Output
- TRP Protocol & Request Vars
- Child Process Runner
- OTLP Output
- Extension Registry API
- shims npm Package
- Insomnia Input Adapter
- StatsD Output
- K6 Driver Setup & Teardown
- Native vs Wasm Driver Parity
- Postman Collection Types
- YAML Serializer
- Chai Assertion Shim
- HTTP Client & TLS Identity
- CLI Commands
- Encoding Module
- Postman State & Assertions
- Metrics Collector Pipeline
- Scheduler Ramp & Pause
- Native vs Wasm Differential
- Open Data Shim
- K6 Module Export Bridge
- Execution Segment Partitioning
- Distributed Agent Protocol
- Adapter/Driver Registry
- Flaky Test Client
- Registration Priority Types
- Postman pm.js Shim
- K6 Driver Bootstrap
- Auth Signers (Basic/AWS/API)
- HTTP Control API
- VU Sources & Recording
- Config File & Env Loading
- Request Auth Header Injection
- Executor Arrival Rate & VUs
- k6-shim Core API
- Adapter Smoke Matrix
- Group Tags & Iteration Timers
- Auth Signer Cache
- Builtins & Protocol Traits
- Engine Orchestration
- Behavior Parity Integration
- VU Pool Management
- Postman Collection Model
- Postman Import Conversion
- Adapter Detect & Import
- Stub Adapter & Driver
- runtime-wasm Smoke
- TypeScript Config
- K6 Script Adapter
- JSON & Crypto Modules
- Wasm Tier Architecture Docs
- CI Jobs & Gates
- Clock Source
- Contributing & CLI Docs
- Signing Schemes
- Cloud K8s Manifests
- Distributed Controller
- Blocking Runtime Workers
- HTTP Response Formatting
- CryptoJS Shim
- Postman Deserializers
- Distributed Runtime Entry
- Runtime Request Execution
- core-wasm Type Definitions
- File Bridge & Local Imports
- Shim Bundle Defaults
- Metric Summary & Thresholds
- SDK Contract & Extensions
- Threshold Parsing & Evaluation
- RPS Limiter
- Scenario Script Items
- Wire Encoding & Outcomes
- TRP Scripting API
- Shim Bundle Compatibility
- x509 Certificate Parsing
- K6 Parity Decisions
- Performance Ceilings
- K6 Request Bridge
- Postman Adapter Detection
- CLI Config Overlay
- JS Bootstrap & VU Context
- Sandbox Config
- Web JS Context Bootstrap
- Outputs & Reporters
- K6 Crypto Shim
- Shim Bundle Smoke
- Honest Numbers & Evidence
- WebSocket Protocol
- SDK Sample & Scenarios
- Bundle Render Script
- Job Config
- Cloud Run CLI
- Summary Export & Reinjection
- Stub Output
- Extra Functions Module
- K6 Socket Shim
- Lodash Shim
- Runtime Publish Scripts
- Knockport & Release Decisions
- Engine Ceilings Assessment
- Controller CLI
- Input Resolution
- HTTP Config Defaults
- ExpectedStatus Parsing
- SDK Test Harness
- Exit Code Integration
- Executors (7 Modes)
- Concurrency & VU Model
- Crate Publishing Order
- K6 Import Stripper
- Collection Validation
- Pacing & Think Time
- Postman Parity Track
- Root Workspace Package
- Agent CLI
- Reporter Snapshots
- HTML Selection Shim
- Wasm Differential CI
- CLI Registry Build
- Engine Error Types
- Multipart Serialization
- Native Bridge Functions
- Hawk Signing Vectors
- Ramp-Down Claims
- Agent Main Entry
- Bridge Functions Doc
- K6 Scenarios Sample
- K6 Response Shim
- Collection Errors
- JS Errors
- Protocol Execute
- K6 Thresholds Sample
- Task Wave Scheme
- SDK Gates Script
- jslib Shim
- Smoke Threshold Fixture
- Bruno Sleep Shim
- Shims Build Script
- Runtime Build Script
- input-wasm Type Definitions
- ExecWasm C ABI
- Core-wasm Build Script
- Wasm Build Scripts
- Version Lockstep Script
- Wasm Size Script
- Invariants & Definition of Done
- CI Paths & Protection
- Aggregation & Thresholds
- HDR Histograms
- Cardinality Cap
- Core-wasm Static Import
- Pause-Gate Fix
- SDK Compile Gates

## God Nodes (most connected - your core abstractions)
1. `VUScheduler` - 81 edges
2. `Request` - 73 edges
3. `JsContext` - 66 edges
4. `MetricsResult` - 50 edges
5. `evaluate_single_threshold()` - 49 edges
6. `Scenario` - 48 edges
7. `ExtensionRegistry` - 44 edges
8. `collection_to_scenario()` - 42 edges
9. `parse_collection_str()` - 41 edges
10. `strip_types()` - 37 edges

## Surprising Connections (you probably didn't know these)
- `Behavioural metrics gate (assert on metrics, not exit code)` --semantically_similar_to--> `W1 - make the numbers trustworthy`  [INFERRED] [semantically similar]
  .github/workflows/ci.yml → TROPEL_MASTER_TODO.md
- `tropel-sdk 0.2.0 companion leaf` --semantically_similar_to--> `SDK leaf property (zero tropel-* deps)`  [INFERRED] [semantically similar]
  CHANGELOG.md → crates/tropel-sdk/README.md
- `ScenarioRunner (tropel-runtime)` --conceptually_related_to--> `Execution Modes`  [INFERRED]
  CHANGELOG.md → docs/executors.md
- `AuthSigner trait and auth schemes (tropel-auth)` --conceptually_related_to--> `Tropel - high-performance load-testing framework`  [INFERRED]
  CHANGELOG.md → README.md
- `Thread-per-core VU model` --semantically_similar_to--> `Thread-per-core VU model`  [INFERRED] [semantically similar]
  README.md → docs/executors.md

## Import Cycles
- 1-file cycle: `crates/tropel-metrics/src/lib.rs -> crates/tropel-metrics/src/lib.rs`
- 2-file cycle: `crates/tropel-report/src/lib.rs -> crates/tropel-report/src/stdout.rs -> crates/tropel-report/src/lib.rs`
- 2-file cycle: `crates/tropel-report/src/csv_reporter.rs -> crates/tropel-report/src/lib.rs -> crates/tropel-report/src/csv_reporter.rs`
- 2-file cycle: `crates/tropel-report/src/json_reporter.rs -> crates/tropel-report/src/lib.rs -> crates/tropel-report/src/json_reporter.rs`
- 4-file cycle: `crates/tropel-auth/src/signers.rs -> crates/tropel-engine/src/engine.rs -> crates/tropel-engine/src/vu_loop.rs -> crates/tropel-http/src/client.rs -> crates/tropel-auth/src/signers.rs`
- 5-file cycle: `crates/tropel-auth/src/signers.rs -> crates/tropel-engine/src/engine.rs -> crates/tropel-engine/src/vu_loop.rs -> crates/tropel-engine/src/js_bootstrap.rs -> crates/tropel-http/src/client.rs -> crates/tropel-auth/src/signers.rs`
- 5-file cycle: `crates/tropel-auth/src/signers.rs -> crates/tropel-engine/src/engine.rs -> crates/tropel-engine/src/vu_loop.rs -> crates/tropel-runtime/src/runner.rs -> crates/tropel-http/src/client.rs -> crates/tropel-auth/src/signers.rs`

## Hyperedges (group relationships)
- **4096-thread concurrency ceiling and its escape paths** — readme_max_workers, tropel_master_todo_layer3, tropel_master_todo_ph [INFERRED 0.85]
- **WASM browser-slice tier (tropel-web, plugin runtime, differential harness)** — _github_workflows_ci_wasm, docs_extensions_wasm_plugins, crates_tropel_web_native_vs_wasm [INFERRED 0.80]
- **crates.io runtime publish set on the tropel-sdk leaf** — changelog_dependency_chain, changelog_scenario_runner, changelog_tropel_sdk [INFERRED 0.85]
- **knockport consumes the core/input/shims wasm packages** — tropel_plan_context_knockport_engine, packages_core_wasm_readme_core_wasm, packages_input_wasm_readme_input_wasm, packages_shims_readme_shims [INFERRED 0.85]
- **Two-tier wasm split: eager core + lazy import + script tier** — packages_core_wasm_readme_core_wasm, packages_input_wasm_readme_input_wasm, packages_core_wasm_readme_two_tier_wasm, tropel_plan_context_eager_lazy_tier [INFERRED 0.85]
- **W0 packaging P0s (TR-008/009/010) block knockport build** — tropel_plan_tasks_w0_stop_the_bleeding_tr008, tropel_plan_tasks_w0_stop_the_bleeding_tr009, tropel_plan_tasks_w0_stop_the_bleeding_tr010 [EXTRACTED 1.00]

## Communities (204 total, 28 thin omitted)

### Community 0 - "HTTP Client & DNS Cache"
Cohesion: 0.05
Nodes (82): Addrs, blacklist_rejects_ip_literal_but_not_hostname(), apply_policy(), bad_blacklist_is_skipped(), box_addrs(), cache_eviction_prefers_forever_entries(), cache_evicts_expired_and_bounded(), cache_get() (+74 more)

### Community 1 - "K6 HTTP Bridge & Driver"
Cohesion: 0.04
Nodes (83): compress_k6_body(), ctx_with_base_shims(), intern_method(), intern_status(), oversized_binary_body_degrades_to_status0_envelope_not_panic(), oversized_text_body_degrades_to_status0_envelope_not_empty_string(), read_export_for_test(), Self (+75 more)

### Community 2 - "OAuth2 Authorization Flow"
Cohesion: 0.06
Nodes (80): attach_token(), attach_token_header_and_query(), auth_params(), authorize_url_carries_everything(), authorize_url_generates_state_when_absent(), AuthorizeParams, AuthorizeRequest, basic_auth_header_encodes_credentials() (+72 more)

### Community 3 - "JS Context & Console Shims"
Cohesion: 0.07
Nodes (56): adjust_error_lines(), CachedScript, compile_global_bytecode_runs_in_fresh_context(), compile_global_bytecode_surfaces_compile_errors(), console_args_to_string(), console_info_debug_trace_dir_do_not_throw(), console_log_accepts_objects_and_multiple_args(), console_stringifier_parity() (+48 more)

### Community 4 - "Engine Extension Registry"
Cohesion: 0.07
Nodes (60): Engine, Default, aot_compile(), atomic_write(), build_item_tree(), build_link_strategy(), cache_key(), clamp_memory_type() (+52 more)

### Community 5 - "HTTP Body & Headers Types"
Cohesion: 0.06
Nodes (47): Clone, Body, body_roundtrip_preserves_all_variants(), body_text_lossy_and_empty_string(), CertificateConfig, Cookie, de_headers(), de_urlencoded_fields() (+39 more)

### Community 6 - "Prometheus Output"
Cohesion: 0.06
Nodes (60): configure_adopts_job_url(), prometheus_factory(), PrometheusOutput, registers_prometheus_output(), AtomicBool, Box, Default, Instant (+52 more)

### Community 7 - "OpenAPI Spec Normalization"
Cohesion: 0.08
Nodes (68): build_request_body(), extract_param_value(), generate_schema_example(), is_swagger2(), normalize_security_definitions(), normalize_swagger2(), normalize_swagger2_operation(), OasComponents (+60 more)

### Community 8 - "JS Transpiler & Virtual Specifiers"
Cohesion: 0.06
Nodes (64): Allocator, Cell, Path, Result, String, transpile_file(), assert_reparses(), decorator_options() (+56 more)

### Community 9 - "Metrics Aggregation Collector"
Cohesion: 0.10
Nodes (50): BTreeMap, Aggregator, config_needs_histograms(), golden_numbers_every_result_field_exact(), k6_default_trend_stats(), merge_fails_on_corrupt_base64_histogram(), merge_fails_on_truncated_v2_bytes(), merge_roundtrip_clean_snapshots_losslessly() (+42 more)

### Community 10 - "Threshold Evaluation Engine"
Cohesion: 0.07
Nodes (66): abort_config(), abort_does_not_fire_when_metric_has_no_samples_yet(), abort_fires_on_breach_with_abort_on_fail(), abort_ignores_non_abort_thresholds(), abort_only_fires_for_actually_breached_thresholds(), abort_respects_delay_abort_eval_grace_period(), checks_unknown_stat_fails_closed(), compound_and_binds_tighter_than_or() (+58 more)

### Community 11 - "Dynamic Variable Catalog"
Cohesion: 0.09
Nodes (59): capitalize_first_letter(), capped_len(), chrono_now(), chrono_now_iso(), DynamicCatalog, epoch_secs(), PredefinedVariableMeta, random_choice() (+51 more)

### Community 12 - "Browser WASI Shim (core-wasm)"
Cohesion: 0.07
Nodes (35): BrowserWasiShim, createExecWasm(), defaultShimSource, ExecWasm, ExecWasmOptions, initialize(), lineBuffered(), NodeWasi (+27 more)

### Community 13 - "Cargo Build & Extension Deps"
Cohesion: 0.07
Nodes (43): build(), BuildConfig, expand_tilde(), expand_tilde_resolves_home(), ExtensionDep, generate_cargo_toml(), generate_cargo_toml_falls_back_to_registry_engine_without_workspace(), generate_cargo_toml_uses_local_engine_path_with_workspace() (+35 more)

### Community 14 - "WebAssembly Host Context"
Cohesion: 0.07
Nodes (47): AsContext, http_request_host(), metric_add_host(), push_iteration_sample(), read_mem_string(), Arc, Box, Caller (+39 more)

### Community 15 - "gRPC Codec & Bounded Cache"
Cohesion: 0.07
Nodes (49): Channel, Codec, body_to_json(), bounded_cache_evicts_oldest_fifo(), bounded_cache_zero_capacity_evicts_on_every_insert(), cache_insert_bounded(), collect_messages(), compile_proto() (+41 more)

### Community 16 - "HTTP Digest Authentication"
Cohesion: 0.09
Nodes (51): base_url(), bracket_host(), build_digest_authorization(), canonical_host(), crypto_nonce_is_unique_hex_and_full_width(), default_service(), derive_signing_key(), digest_matches_rfc2617_md5_reference_vector() (+43 more)

### Community 17 - "WebSocket Integration Tests"
Cohesion: 0.07
Nodes (45): binary_messages(), connection_refused_is_error(), make_request(), multiple_messages_via_config(), Option, SocketAddr, spawn_echo_server(), spawn_echo_then_close_server() (+37 more)

### Community 18 - "JSON Path & Escape Resolver"
Cohesion: 0.13
Nodes (39): EscapeMode, json_escape(), placeholder_in_json_string(), Arc, Default, HashMap, Regex, Self (+31 more)

### Community 19 - "HAR Input Adapter"
Cohesion: 0.09
Nodes (40): build_body(), generate_item_name(), har_entry_to_item(), HarEntry, HarHeader, HarInputAdapter, HarLog, HarPostData (+32 more)

### Community 20 - "K6 Options & Threshold Conversion"
Cohesion: 0.11
Nodes (40): build_threshold(), K6Dns, K6Options, K6Scenario, K6Stage, K6ThresholdSpec, parse(), HashMap (+32 more)

### Community 21 - "Runtime Smoke & JWT Claims"
Cohesion: 0.07
Nodes (45): RFC-3339, [a, b], att, attQ, auth, bytes, claim, customHeader (+37 more)

### Community 22 - "VU Control Spawn Guards"
Cohesion: 0.07
Nodes (16): control_grow_adds_delta_not_absolute_store(), ControlSpawnGuard, ControlSpawnGuard<'a>, IdleVusGuard, IdleVusGuard<'a>, Arc, AtomicBool, AtomicU32 (+8 more)

### Community 23 - "Crypto Module (AES/HMAC)"
Cohesion: 0.12
Nodes (41): aes_cbc_decrypt(), aes_cbc_encrypt(), aes_gcm_decrypt(), aes_gcm_encrypt(), evp_bytes_to_key(), hmac_dispatch(), hmac_md5(), hmac_sha1() (+33 more)

### Community 24 - "Browser WASI Shim Package"
Cohesion: 0.05
Nodes (42): @bjorn3/browser_wasi_shim, dependencies, @tropel/shims, description, devDependencies, typescript, engines, node (+34 more)

### Community 25 - "Metric Registry & Units"
Cohesion: 0.09
Nodes (31): BufWriter, clear(), clear_forgets_previous_runs_declarations(), lock_registry(), MetricUnit, register(), unit_of(), unit_of_classifies_by_heuristic_and_declaration() (+23 more)

### Community 26 - "Postman Collection Parser"
Cohesion: 0.13
Nodes (40): collection_to_scenario(), collection_to_scenario_seeds_env_vars_with_postman_precedence(), empty_folder_with_scripts_not_emitted_as_pseudo_request(), folder_with_only_empty_subfolders_not_emitted(), parse_collection_str(), HashMap, test_collection_and_folder_scripts_reach_leaves_in_order(), test_collection_auth_inherited_by_requests() (+32 more)

### Community 27 - "input-wasm npm Package"
Cohesion: 0.05
Nodes (39): description, engines, node, exports, ./glue, ./wasm/tropel_input_wasm_bg.wasm, files, default (+31 more)

### Community 28 - "HTTP File Input Adapter"
Cohesion: 0.10
Nodes (27): block_to_item(), generate_item_name(), HttpFileAdapter, merge_header(), parse_header_line(), parse_request_line(), parse_variable(), pick_body() (+19 more)

### Community 29 - "Reqwest Client & Redirects"
Cohesion: 0.11
Nodes (30): bodies_sent_for_all_methods_including_delete_options_trace_custom(), body_size(), body_to_bytes(), body_to_reqwest(), client_level_request_timeout_bounds_hung_server(), cross_origin_redirect_strips_signed_authorization(), discarded_body_still_reuses_pooled_connection(), follow_redirects_false_returns_redirect_not_followed() (+22 more)

### Community 30 - "Bruno Input Adapter"
Cohesion: 0.14
Nodes (31): BruApiKey, BruAuth, BruBasic, BruBearer, BruBody, BruCollection, BruDigest, BruEnvironment (+23 more)

### Community 31 - "InfluxDB Output"
Cohesion: 0.11
Nodes (27): encodes_line_protocol(), escapes_special_chars(), flush_sends_datagram(), http_lines_carry_ns_timestamp_udp_lines_do_not(), InfluxdbOutput, InfluxTarget, parse_http_target(), parses_http_v1_target_from_path() (+19 more)

### Community 32 - "Postman Script Runner"
Cohesion: 0.18
Nodes (32): build_scope_sees_pm_environment_set_values(), each_script_runs_in_its_own_lexical_scope(), failed_request_clears_stale_pm_response(), flatten_execution_items(), flatten_execution_items_descends_folders_in_order(), flatten_execution_items_skips_scriptless_empty_leaves(), folder_leaf_prerequest_runs_and_value_resolves(), force_stop_flag_breaks_item_loop() (+24 more)

### Community 33 - "core-wasm npm Package"
Cohesion: 0.05
Nodes (37): description, engines, node, exports, ./glue, ./wasm/tropel_core_wasm_bg.wasm, files, default (+29 more)

### Community 34 - "gRPC Streaming Integration"
Cohesion: 0.13
Nodes (26): bidi_streaming_roundtrip(), BidiHandler, client_streaming_roundtrip(), ClientStreamHandler, GreeterService, make_req(), Arc, Context (+18 more)

### Community 35 - "CSV Reporter"
Cohesion: 0.09
Nodes (24): Box, Vec, CsvReporter, Option, PathBuf, Result, Self, String (+16 more)

### Community 36 - "Worker Join & Busy Guards"
Cohesion: 0.13
Nodes (25): BusyGuard, drop_detaches_wedged_worker_within_join_bound(), drop_joins_healthy_workers_promptly(), Arc, AtomicBool, AtomicUsize, Drop, Duration (+17 more)

### Community 37 - "VU Loop Sampler Tasks"
Cohesion: 0.15
Nodes (34): dropped_sampler_task(), Arc, AtomicU32, AtomicU64, Box, Drop, Duration, F (+26 more)

### Community 38 - "Execution Config & Arrival Rate"
Cohesion: 0.11
Nodes (20): extract_think_time(), ArrivalRateStage, default_start_time(), default_time_unit(), ExecutionConfig, from_mode_maps_k6_modes_with_defaults(), output_into_worker_nulls_streaming_fields(), OutputConfig (+12 more)

### Community 39 - "Latency Histogram"
Cohesion: 0.12
Nodes (13): auto_resizing_histogram(), garbage_bounds_fall_back_to_auto_resize(), LatencyHistogram, merge_is_exact_sum_of_buckets(), out_of_range_samples_upgrade_instead_of_truncating_population(), percentiles_track_ground_truth(), Default, Duration (+5 more)

### Community 40 - "Live State Output"
Cohesion: 0.13
Nodes (22): live_state_counts_metrics_and_tracks_vus(), live_state_render_fixed_duration_shows_bar_and_pct(), live_state_render_no_duration_elapsed_only(), live_state_rolling_p95_and_max_bounded_window(), LiveState, Default, Duration, Instant (+14 more)

### Community 41 - "TRP Protocol & Request Vars"
Cohesion: 0.14
Nodes (29): decode_json_encoded(), decode_json_value(), parse_headers(), parse_headers_non_string_object_values_are_stringified(), parse_method(), resolve_send_request(), resolve_vars(), response_json_string() (+21 more)

### Community 42 - "Child Process Runner"
Cohesion: 0.13
Nodes (25): Child, kill_and_join(), Duration, JoinHandle, Option, Path, Receiver, Result (+17 more)

### Community 43 - "OTLP Output"
Cohesion: 0.12
Nodes (23): k6_metric_type(), build_export_request(), counter_aggregates_per_tag_set_with_delta(), export_request_structure(), flush_posts_to_endpoint(), normalize_metrics_url(), OtlpOutput, AtomicUsize (+15 more)

### Community 44 - "Extension Registry API"
Cohesion: 0.14
Nodes (16): ExtensionRegistry, Arc, Box, HashMap, Option, Path, Send, String (+8 more)

### Community 45 - "shims npm Package"
Cohesion: 0.06
Nodes (32): description, engines, node, exports, files, homepage, dist, k6 (+24 more)

### Community 46 - "Insomnia Input Adapter"
Cohesion: 0.14
Nodes (25): build_auth(), build_body(), build_items(), ExportRoot, InsomniaAuth, InsomniaBody, InsomniaBodyParam, InsomniaHeader (+17 more)

### Community 47 - "StatsD Output"
Cohesion: 0.12
Nodes (21): String, Vec, TagPolicy, encodes_datadog_format(), flush_sends_datagram(), AtomicUsize, Cow, Into (+13 more)

### Community 48 - "K6 Driver Setup & Teardown"
Cohesion: 0.18
Nodes (26): eval_module_call_export(), interned(), is_typescript_ext(), k6_error_code(), K6Driver, push_http_failure(), push_http_samples(), push_http_samples_for() (+18 more)

### Community 49 - "Native vs Wasm Driver Parity"
Cohesion: 0.13
Nodes (20): native_k6_and_wasm_driver_produce_identical_http_metrics(), per_url_for(), Option, Result, SocketAddr, String, run_one_iteration(), safe_tag() (+12 more)

### Community 50 - "Postman Collection Types"
Cohesion: 0.21
Nodes (28): AuthAttribute, BodyOptions, CollectionAuth, CollectionInfo, FileSpec, FolderItem, FormParameter, GraphQLSpec (+20 more)

### Community 51 - "YAML Serializer"
Cohesion: 0.12
Nodes (12): args_are_always_quoted(), block_indents_from_key_column(), double_quote(), is_yaml_resolvable(), kv_plain_emits_fixed_literals_verbatim(), plain_safe(), Display, Self (+4 more)

### Community 52 - "Chai Assertion Shim"
Cohesion: 0.08
Nodes (3): Assertion(), assertTypeMatches(), chaiTypeName()

### Community 53 - "HTTP Client & TLS Identity"
Cohesion: 0.21
Nodes (16): CertClientMap, check_literal_blacklist(), HttpClient, is_credential_header(), parse_duration(), parse_tls_version(), Arc, Client (+8 more)

### Community 54 - "CLI Commands"
Cohesion: 0.15
Nodes (24): Cli, archive_command(), build_custom(), inspect_command(), list_extensions(), load_data_file(), print_scenario_summary(), print_version() (+16 more)

### Community 55 - "Encoding Module"
Cohesion: 0.17
Nodes (22): base64_decode(), base64_encode(), base64url_decode(), base64url_encode(), EncodingModule, hex_decode(), hex_encode(), Result (+14 more)

### Community 56 - "Postman State & Assertions"
Cohesion: 0.17
Nodes (17): AssertionCounters, attach_exec_context_wires_shared_atomics(), PmState, record_test_tagged_carries_extra_tags(), record_test_updates_counters_and_emits_check_sample(), Arc, AtomicU32, AtomicU64 (+9 more)

### Community 57 - "Metrics Collector Pipeline"
Cohesion: 0.12
Nodes (13): MetricKey, MetricsCollector, Arc, AtomicBool, Default, Mutex, Receiver, Sample (+5 more)

### Community 58 - "Scheduler Ramp & Pause"
Cohesion: 0.13
Nodes (19): arm_ramp_down_step_accumulates_slots_across_steps(), await_handles_bounded_skips_already_polled_handles(), control_max_clamped_to_configured_ceiling(), fine_grained_ramp_up_completes_in_stage_duration(), graceful_stop_duration(), malformed_time_unit_fails_validation(), parse_duration(), pause_is_level_triggered_and_independent() (+11 more)

### Community 59 - "Native vs Wasm Differential"
Cohesion: 0.14
Nodes (25): fixture_response(), fixture_run_request(), host_http(), host_shim(), HostState, native_and_wasm_runtime_produce_identical_outcomes(), native_outcome_postcard_roundtrip(), norm_sample() (+17 more)

### Community 60 - "Open Data Shim"
Cohesion: 0.14
Nodes (7): makeSharedIterator(), open(), openDataBase64ToBytes(), NOTE: declared as top-level functions (NOT inside `if` guards) — keeps, NOTE: this helper is deliberately private-named (openDataBase64ToBytes), SharedArrayView(), wrap()

### Community 61 - "K6 Module Export Bridge"
Cohesion: 0.25
Nodes (22): build_k6_response_object(), call_module_export(), call_module_handle_summary(), eval_module_export_json(), eval_module_handle_summary(), http_tags(), http_tags_for(), k6_deadline() (+14 more)

### Community 62 - "Execution Segment Partitioning"
Cohesion: 0.14
Nodes (9): apply_scales_execution_config(), ExecutionSegment, parses_segment_spec(), Option, Result, Self, Vec, scales_vus_and_iterations() (+1 more)

### Community 63 - "Distributed Agent Protocol"
Cohesion: 0.13
Nodes (23): connect_with_retry(), Result, TcpStream, run_agent(), AssignMsg, frame_rejects_oversized(), frame_roundtrip(), generate_token() (+15 more)

### Community 64 - "Adapter/Driver Registry"
Cohesion: 0.16
Nodes (15): factory_registration_takes_precedence_over_adapter_registration(), instantiate_protocols_returns_scheme_keyed_map(), list_inputs_merges_factories_and_dedups(), register_and_get_by_id_all_four_kinds(), resolve_driver_uses_content_detection(), resolve_input_claims_by_content_prefix(), resolve_input_priority_wins_over_registration_order(), IndexMap (+7 more)

### Community 65 - "Flaky Test Client"
Cohesion: 0.17
Nodes (16): FlakyClient, Arc, AtomicBool, AtomicU32, AtomicU64, Box, Duration, HashMap (+8 more)

### Community 66 - "Registration Priority Types"
Cohesion: 0.18
Nodes (14): DriverRegistration, InputAdapterRegistration, OutputRegistration, ProtocolRegistration, Box, Self, stub_adapter(), stub_driver() (+6 more)

### Community 67 - "Postman pm.js Shim"
Cohesion: 0.10
Nodes (7): NOTE: a body that fails AFTER an await (e.g. `await fetch(); pm.expect(...)`), __tropel_build_binding(), buildMultipartBody(), escapeMultipartFieldName(), guardChain(), isChainMember(), pm

### Community 68 - "K6 Driver Bootstrap"
Cohesion: 0.12
Nodes (19): bootstrap_js_libs(), K6DriverInstance, K6ExecState, K6ModuleLoader, parse_headers_tolerant(), AtomicBool, AtomicU64, Instant (+11 more)

### Community 69 - "Auth Signers (Basic/AWS/API)"
Cohesion: 0.11
Nodes (12): ApiKeyAuth, AwsSigV4Auth, BasicAuth, build_auth_signer(), build_auth_signer_covers_all_variants(), HawkAuth, OAuth1Auth, OAuth2Auth (+4 more)

### Community 70 - "HTTP Control API"
Cohesion: 0.16
Nodes (17): handle_conn(), oversized_body_rejected_before_alloc(), parse_status_body(), route(), route_stopped_false_is_noop(), route_stopped_true_requests_stop(), Arc, Option (+9 more)

### Community 71 - "VU Sources & Recording"
Cohesion: 0.15
Nodes (17): driver_source_threads_protocols_into_vu_context(), DriverVuSource, RecordingInstance, Arc, AtomicBool, Box, HashMap, Option (+9 more)

### Community 72 - "Config File & Env Loading"
Cohesion: 0.15
Nodes (19): env_execution(), env_num(), env_str(), PartialConfig, HashMap, Option, Path, Result (+11 more)

### Community 73 - "Request Auth Header Injection"
Cohesion: 0.24
Nodes (19): apikey_header_and_query(), auth_header(), basic_base64s_credentials(), bearer_sets_header(), build_request(), digest_challenge_response_parses_and_computes(), digest_challenge_response_works_when_digest_second_scheme(), digest_cnonce_varies_between_requests() (+11 more)

### Community 74 - "Executor Arrival Rate & VUs"
Cohesion: 0.20
Nodes (12): arrival_rate_grows_pool_to_keep_up_with_latency(), arrival_rate_never_drops_with_10_vus_at_300ms(), arrival_rate_respawns_vu_that_dies_mid_run(), arrival_test_vu(), constant_respawns_vu_that_dies_mid_run(), idle_guard_restores_count_and_mark_busy_saturates(), ramp_down_rearms_surplus_from_real_active_not_stage_start(), ramping_hold_respawns_vu_that_dies_mid_stage() (+4 more)

### Community 75 - "k6-shim Core API"
Cohesion: 0.11
Nodes (9): base64ToBytes(), k6HTTPRequest(), normalizeK6Request(), NOTE: uses `var` assignment (NOT `function` inside the guard) — using `var`, NOTE: `var` assignments, not `function` declarations — avoids re-declaring, NOTE: the `crypto` object itself is intentionally NOT exposed as a bare, NOTE: each fallback declares the binding with `var` inside the block. A, NOTE: ids are snapshotted, so a timer REGISTERED inside a callback only (+1 more)

### Community 76 - "Adapter Smoke Matrix"
Cohesion: 0.12
Nodes (16): bru, har, insomnia, openapi, postman, s1, s2, s3 (+8 more)

### Community 77 - "Group Tags & Iteration Timers"
Cohesion: 0.18
Nodes (18): Box, test_check_tags_accept_non_string_values(), test_cross_iteration_settimeout_fires(), test_custom_metric_add_drops_non_finite_values(), test_custom_metric_inside_group_carries_group_path(), test_driver_pumps_timers_at_iteration_boundary(), test_http_in_nested_group_carries_full_path(), test_nested_group_tags_use_full_path() (+10 more)

### Community 78 - "Auth Signer Cache"
Cohesion: 0.14
Nodes (13): AuthSigner, Send, Sync, DriverHttpClientImpl, Result, auth_cache_key(), Box, Mutex (+5 more)

### Community 79 - "Builtins & Protocol Traits"
Cohesion: 0.18
Nodes (14): link_builtins(), register_builtins(), ProtocolOutcome, Arc, HashMap, Option, Sample, Self (+6 more)

### Community 80 - "Engine Orchestration"
Cohesion: 0.15
Nodes (15): emit_handle_summary(), EngineResult, Duration, HashMap, Instant, Option, Result, Self (+7 more)

### Community 81 - "Behavior Parity Integration"
Cohesion: 0.24
Nodes (19): connection_refused_is_recorded_as_failure(), force_stop_interrupts_sleeping_vu(), hung_server_is_bounded_by_global_request_timeout(), hung_server_is_bounded_by_request_timeout(), k6_script_records_requests_checks_and_real_latency(), non_2xx_drives_http_req_failed(), ramping_stages_span_wall_clock_and_reach_target(), Arc (+11 more)

### Community 82 - "VU Pool Management"
Cohesion: 0.37
Nodes (5): Duration, F, JoinHandle, Option, Vec

### Community 83 - "Postman Collection Model"
Cohesion: 0.18
Nodes (15): collection_roundtrip_preserves_forms(), default_method(), description_accepts_string_and_object_forms(), folder_first_discrimination_with_stray_request_key(), folder_first_tolerates_stray_malformed_request_object(), header_without_value_defaults_to_empty(), malformed_example_cookie_does_not_fail_collection(), method_defaults_to_get() (+7 more)

### Community 84 - "Postman Import Conversion"
Cohesion: 0.23
Nodes (19): Event, assemble_url(), build_query_params(), build_url(), convert_auth(), convert_body(), convert_items(), convert_request() (+11 more)

### Community 85 - "Adapter Detect & Import"
Cohesion: 0.23
Nodes (17): detect(), detect_impl(), err(), err_text(), import_any(), import_any_dispatches_and_round_trips(), import_any_impl(), import_by_id() (+9 more)

### Community 86 - "Stub Adapter & Driver"
Cohesion: 0.12
Nodes (9): Option, Path, Result, Sample, Value, StubAdapter, StubDriver, StubOutput (+1 more)

### Community 87 - "runtime-wasm Smoke"
Cohesion: 0.11
Nodes (14): artifact, checks, __dirname, expectedSeen, failedChecks, failedReqs, match, outcome (+6 more)

### Community 88 - "TypeScript Config"
Cohesion: 0.11
Nodes (18): compilerOptions, declaration, esModuleInterop, forceConsistentCasingInFileNames, lib, module, moduleResolution, noUnusedLocals (+10 more)

### Community 89 - "K6 Script Adapter"
Cohesion: 0.15
Nodes (6): build_scenario_from_source(), is_postman_collection(), K6ScriptAdapter, Option, Path, Result

### Community 90 - "JSON & Crypto Modules"
Cohesion: 0.20
Nodes (12): CryptoModule, json_get(), json_parse(), json_stringify(), JsonModule, Option, Result, String (+4 more)

### Community 91 - "Wasm Tier Architecture Docs"
Cohesion: 0.14
Nodes (18): API_CLIENT_WEB_PAYLOAD.md, @tropel/core-wasm core tier, DynamicCatalog dynamic-variable catalog, tropel-web script tier, Two-tier wasm split (core + script), KnockPort WEB_EXTENSION_RUNTIME_SPLIT.md, @tropel/input-wasm lazy import tier, @tropel/runtime-wasm (+10 more)

### Community 92 - "CI Jobs & Gates"
Cohesion: 0.21
Nodes (17): cargo-deny advisories gate, cargo-outdated (informational), Audit Workflow (dependency advisories), CI OK aggregate gate, clippy -D warnings job, cargo-deny job (advisories/license/bans), rustfmt job, MSRV 1.94 gate (+9 more)

### Community 93 - "Clock Source"
Cohesion: 0.21
Nodes (14): ClockSource, base(), monotonic_now_nanos(), monotonic_wall_now(), now(), now_nanos_is_monotonic(), Instant, SystemTime (+6 more)

### Community 94 - "Contributing & CLI Docs"
Cohesion: 0.15
Nodes (17): Contributor Covenant Code of Conduct, Contributing to Tropel, Testing practices (insta, wiremock), tropel archive command, tropel build command, CLI Reference, tropel inspect command, Agent (workload fraction) (+9 more)

### Community 95 - "Signing Schemes"
Cohesion: 0.23
Nodes (9): BearerAuth, generate_nonce(), insert_header(), is_form_urlencoded(), nonce_is_unique_and_hex(), Result, set_auth_header(), Request (+1 more)

### Community 96 - "Cloud K8s Manifests"
Cohesion: 0.21
Nodes (15): cloud_local_runs_and_merges(), generate_k8s_manifests(), manifests_contain_full_topology(), manifests_defaults(), manifests_embed_input_file_and_rewrite_input_path(), manifests_embed_token_and_mount_for_agents(), manifests_keep_unreadable_input_as_is(), manifests_quote_hostile_values() (+7 more)

### Community 97 - "Distributed Controller"
Cohesion: 0.23
Nodes (15): agent_timeout(), controller_errors_on_bad_sequence(), controller_rejects_bad_token(), distributed_two_agents_merge_losslessly(), parse_duration(), read_agent_snapshot(), Duration, Instant (+7 more)

### Community 98 - "Blocking Runtime Workers"
Cohesion: 0.18
Nodes (13): execute_blocking(), execute_blocking_propagates_error(), execute_blocking_resolves_future(), execute_blocking_tight_loop_no_starvation(), execute_blocking_works_from_inside_current_thread_runtime(), io_rt(), io_worker_threads(), F (+5 more)

### Community 99 - "HTTP Response Formatting"
Cohesion: 0.14
Nodes (13): canonical_header_name(), escape_multipart_field_name(), format_http_version(), HttpResponse, Duration, From, HashMap, OnceLock (+5 more)

### Community 100 - "CryptoJS Shim"
Cohesion: 0.18
Nodes (5): bytesFromWordArray(), deriveKeyAndIv(), Hasher(), WordArray(), wordArrayFromBytes()

### Community 101 - "Postman Deserializers"
Cohesion: 0.32
Nodes (13): de_auth_attrs(), de_exec(), de_opt_code(), de_opt_description(), de_opt_request(), de_opt_response_time(), de_presence(), D (+5 more)

### Community 102 - "Distributed Runtime Entry"
Cohesion: 0.22
Nodes (13): build_runtime(), distributed_workers_from_override(), has_token_source(), report_and_thresholds(), report_and_thresholds_honors_summary_export(), resolve_token(), Instant, Option (+5 more)

### Community 103 - "Runtime Request Execution"
Cohesion: 0.22
Nodes (13): body_custom_serde_survives_postcard(), encode_fatal(), Result, RunOutcome, RunRequest, run_request(), run_request_produces_samples(), run_request_sync() (+5 more)

### Community 104 - "core-wasm Type Definitions"
Cohesion: 0.12
Nodes (15): AuthorizeParams, AuthorizeRequest, DecodedJwt, InitCoreWasmOptions, OAuth2GrantType, OauthError, PkcePair, PredefinedVariableMeta (+7 more)

### Community 105 - "File Bridge & Local Imports"
Cohesion: 0.21
Nodes (15): ctx_with_file_bridges(), file_cache(), install_iteration_global(), register_k6_file_bridges(), PathBuf, temp_script_dir(), test_module_local_import_missing_file_errors(), test_module_local_import_resolves_to_disk() (+7 more)

### Community 106 - "Shim Bundle Defaults"
Cohesion: 0.21
Nodes (9): Cow, Default, Path, Self, String, Vec, shim_lists_stay_in_lockstep_with_bru(), ShimBundle (+1 more)

### Community 107 - "Metric Summary & Thresholds"
Cohesion: 0.30
Nodes (15): MetricSummary, parse_percentile(), percentile_value(), stat_needs_histogram(), trend_stat_value(), aggregate_series(), counter_rate(), evaluate_single_threshold_opt() (+7 more)

### Community 108 - "SDK Contract & Extensions"
Cohesion: 0.19
Nodes (15): Tropel SDK readme, InputAdapter trait (detect/parse), inventory registration (compile-time discovery), wit/adapter.wit tropel-adapter world, Extensions, SDK contract (Scenario/InputAdapter/Driver), tropel-x-grpc extension, WASM plugin tier (wasmtime) (+7 more)

### Community 109 - "Threshold Parsing & Evaluation"
Cohesion: 0.24
Nodes (14): brace_aware_split(), check_abort_on_fail(), compound_clauses(), evaluate_thresholds(), is_known_stat(), parse_duration(), Duration, HashMap (+6 more)

### Community 110 - "RPS Limiter"
Cohesion: 0.26
Nodes (9): extreme_rates_never_panic(), no_burst_after_idle(), paces_requests_at_rate(), RpsLimiter, AtomicU64, Duration, Instant, Self (+1 more)

### Community 111 - "Scenario Script Items"
Cohesion: 0.26
Nodes (11): folder(), script_item(), Result, HashMap, Option, String, Value, Vec (+3 more)

### Community 112 - "Wire Encoding & Outcomes"
Cohesion: 0.24
Nodes (10): HashMap, Into, Option, Result, Self, String, Vec, RunOutcome (+2 more)

### Community 113 - "TRP Scripting API"
Cohesion: 0.21
Nodes (13): Postman pm.* API, trp.collectionVariables, trp.environment, trp.execution flow control, trp.globals, trp.group, trp.metrics, pm.* Postman compat layer (+5 more)

### Community 114 - "Shim Bundle Compatibility"
Cohesion: 0.15
Nodes (13): bru.* Bruno compat layer, trp.test / trp.expect assertions, bru-shim, chai-shim, cryptojs-shim, exec-shim, crates/tropel-engine/src/js_bootstrap.rs, lodash-shim (+5 more)

### Community 115 - "x509 Certificate Parsing"
Cohesion: 0.24
Nodes (13): k6B64Decode(), k6Utf8Decode(), x509AltNames(), x509IssuerObject(), x509ParseCert(), x509ParseExtensions(), x509ParseName(), x509ParseTime() (+5 more)

### Community 116 - "K6 Parity Decisions"
Cohesion: 0.17
Nodes (13): D4 k6 parity conformance decision, k6 alternative consumer, TROPEL_PARITY_K6.md, TROPEL_PARITY_POSTMAN.md, W2 k6 parity, TR-011 restore zero sub-timings, TR-111 no-data threshold clause, TR-121 transport-failure metrics (+5 more)

### Community 117 - "Performance Ceilings"
Cohesion: 0.26
Nodes (13): Layer 2: silent throughput cap, TROPEL_PERF_VS_K6.md, W3 Throughput, W5 structural ceilings, TR-001 dropped-sample counter, TR-002 benchmarks, TR-014 h2 correctness, TR-301 output starvation (+5 more)

### Community 118 - "K6 Request Bridge"
Cohesion: 0.20
Nodes (10): build_k6_request(), coerce_tag_value(), json_to_value(), parse_k6_extras(), register_shared_array_bridges(), Value, shared_array_cache(), stringify_tag_map_into() (+2 more)

### Community 119 - "Postman Adapter Detection"
Cohesion: 0.20
Nodes (4): PostmanInputAdapter, Result, test_parse_simple(), test_parse_string_form_url()

### Community 120 - "CLI Config Overlay"
Cohesion: 0.36
Nodes (11): apply_overlay(), base_config(), merge_partial(), String, test_merge_partial_file_wins_over_env(), test_overlay_env_thresholds_fill_not_override(), test_overlay_execution_ignored_when_cli_flags_explicit(), test_overlay_execution_when_no_cli_load_flags() (+3 more)

### Community 121 - "JS Bootstrap & VU Context"
Cohesion: 0.29
Nodes (11): bootstrap_shims(), create_vu_js_context(), create_vu_js_context_honors_custom_sandbox_config(), js_deadline_secs(), js_heap_bytes(), Arc, AtomicBool, Duration (+3 more)

### Community 122 - "Sandbox Config"
Cohesion: 0.24
Nodes (8): default_config_is_trp_canonical_only(), hostile_names_are_js_escaped(), js_quote(), Default, Self, String, Vec, SandboxConfig

### Community 123 - "Web JS Context Bootstrap"
Cohesion: 0.27
Nodes (11): new_pm_state(), SharedPmState, create_web_js_context(), create_web_js_context_force_stop_interrupts_busy_loop(), create_web_js_context_honors_custom_sandbox_config(), Arc, AtomicBool, Option (+3 more)

### Community 124 - "Outputs & Reporters"
Cohesion: 0.20
Nodes (12): tropel run command, Metrics & Thresholds, Sub-timings (blocked/connecting/tls/sending/waiting/receiving), Thresholds (milliseconds, k6 abort semantics), Dropped data & verification (verified/unverified), Outputs & Reporters, End-of-run reporters (stdout/json/csv), Streaming outputs (Output trait MPSC consumer) (+4 more)

### Community 125 - "K6 Crypto Shim"
Cohesion: 0.21
Nodes (10): k6B64Encode(), k6BytesToHex(), k6CreateHmac(), k6DigestOutput(), K6Hasher(), k6HexEncode(), k6Hmac(), k6OneShotHash() (+2 more)

### Community 126 - "Shim Bundle Smoke"
Cohesion: 0.17
Nodes (10): b64Defs, bundlePath, DEFAULT_ORDER, __dirname, joinedK6, K6_ORDER, nextFn, rendered (+2 more)

### Community 127 - "Honest Numbers & Evidence"
Cohesion: 0.18
Nodes (12): Invariant: a green run is never wrong, Layer 1: green run can be wrong, Evidence grades EXEC/CALC/MEAS/READ, TROPEL_MASTER_TODO.md evidence register, W0 Stop the bleeding, W1 Honest numbers, TR-003 MSRV, TR-004 ExpectedStatus parse (+4 more)

### Community 128 - "WebSocket Protocol"
Cohesion: 0.22
Nodes (8): parse_duration(), Box, Duration, Option, Result, Value, WebSocketProtocol, ws_factory()

### Community 129 - "SDK Sample & Scenarios"
Cohesion: 0.22
Nodes (11): JSONPlaceholder Test API, Post schema, PostInput schema, /posts and /posts/{id} paths, OpenAPI/Postman/HAR import formats, tropel-sdk Scenario JSON shape, D3 tropel-sdk inversion decision, tropel-sdk crate (git submodule) (+3 more)

### Community 130 - "Bundle Render Script"
Cohesion: 0.22
Nodes (8): DEFAULT_ORDER, defaultBundle, here, K6_ORDER, k6Bundle, read(), root, total

### Community 131 - "Job Config"
Cohesion: 0.22
Nodes (8): JobConfig, Default, HashMap, Option, Self, String, Value, Vec

### Community 132 - "Cloud Run CLI"
Cohesion: 0.38
Nodes (9): Args, Cmd, load_config(), main(), Option, PathBuf, Result, String (+1 more)

### Community 133 - "Summary Export & Reinjection"
Cohesion: 0.40
Nodes (9): build_summary_data(), headline_http_req_duration_reinjected_into_handle_summary(), headline_reinjection_never_shadows_per_tag_series(), metric_entry(), HashMap, Instant, String, Value (+1 more)

### Community 134 - "Stub Output"
Cohesion: 0.22
Nodes (4): Result, Sample, StubAdapter, StubOutput

### Community 135 - "Extra Functions Module"
Cohesion: 0.29
Nodes (7): ExtraFunctionsModule, generate_uuid(), random_float(), random_int(), random_int_empty_range_does_not_panic(), Result, String

### Community 138 - "Runtime Publish Scripts"
Cohesion: 0.38
Nodes (9): check_name_available(), check_published_sdk_api(), crate_is_live(), flip_publish_flag(), parse_retry_after(), publish_one(), restore_publish_flags(), retry_epoch() (+1 more)

### Community 139 - "Knockport & Release Decisions"
Cohesion: 0.24
Nodes (10): D1 publish channels decision, D2 version lockstep decision, knockport relationship direction, tropel agent (localhost process), TROPEL_EXEC_SPLIT.md, W4 knockport interface, TR-401 package naming, TR-405 tropel agent (+2 more)

### Community 140 - "Engine Ceilings Assessment"
Cohesion: 0.31
Nodes (9): Behavioural metrics gate (assert on metrics, not exit code), MAX_WORKERS = 4096 concurrency ceiling, Master Assessment & Execution Plan, Fail-closed / assert-user-visible-number discipline, Layer 2 - silent throughput caps, Layer 3 - synchronous host-call concurrency ceiling, P-D - egress throughput cliffs, P-H - the structural ceiling (+1 more)

### Community 141 - "Controller CLI"
Cohesion: 0.33
Nodes (8): Args, main(), Option, PathBuf, Result, String, run(), TcpListener

### Community 142 - "Input Resolution"
Cohesion: 0.31
Nodes (8): resolve_input_or_driver(), ResolvedInput, Arc, Box, HashMap, Option, Result, String

### Community 143 - "HTTP Config Defaults"
Cohesion: 0.25
Nodes (5): default_expected_statuses(), http_config_defaults_match_k6(), HashMap, Self, Vec

### Community 144 - "ExpectedStatus Parsing"
Cohesion: 0.25
Nodes (6): ExpectedStatus, D, Deserialize, Error, Result, status_is_expected()

### Community 146 - "Exit Code Integration"
Cohesion: 0.39
Nodes (8): failed_threshold_exits_nonzero(), passing_threshold_exits_zero(), Path, PathBuf, SocketAddr, run_tropel(), start_echo_server(), write_k6_script()

### Community 147 - "Executors (7 Modes)"
Cohesion: 0.22
Nodes (9): constant-arrival-rate executor, constant-vus executor, Execution Modes, externally-controlled executor, ramping-vus executor, Thread-per-core VU model, Seven executors, Thread-per-core VU model (+1 more)

### Community 148 - "Concurrency & VU Model"
Cohesion: 0.22
Nodes (9): QuickJS Embedding (one context per VU), native_vs_wasm differential harness, Layer 3: structural concurrency cap, MAX_WORKERS = 4096 concurrency ceiling, VU (virtual user), TR-408 differential harness, TR-502 async host calls, TR-503 shared Runtime (+1 more)

### Community 149 - "Crate Publishing Order"
Cohesion: 0.29
Nodes (8): AuthSigner trait and auth schemes (tropel-auth), Crate dependency chain (publication order), Changelog, Runtime publish set 0.1.0, ScenarioRunner (tropel-runtime), tropel-sdk 0.2.0 companion leaf, SDK leaf property (zero tropel-* deps), ExpectedStatus config semantics

### Community 150 - "K6 Import Stripper"
Cohesion: 0.25
Nodes (8): preprocess_k6_source_module(), test_module_preprocess_keeps_exports(), test_module_preprocess_keeps_local_import(), test_module_preprocess_keeps_named_export_block(), test_module_preprocess_strips_import_with_trailing_comment(), test_module_preprocess_strips_jslib_url_import(), test_module_preprocess_strips_multiline_import(), test_module_preprocess_strips_only_k6_reexports()

### Community 151 - "Collection Validation"
Cohesion: 0.39
Nodes (8): Collection, CollectionItem, Box, Deserialize, parse_collection(), Result, validate_collection(), validate_methods()

### Community 152 - "Pacing & Think Time"
Cohesion: 0.43
Nodes (7): apply_think_time(), parse_duration_str(), Arc, Duration, Notify, Option, Result

### Community 153 - "Postman Parity Track"
Cohesion: 0.25
Nodes (8): TROPEL_PARITY_POSTMAN.md, Human sign-off gate, W6 Release mechanics, TR-263 local-file read, Postman parity track, TR-601 cargo publish, TR-602 tropel-auth decision, TR-603 auth correctness

### Community 154 - "Root Workspace Package"
Cohesion: 0.25
Nodes (7): description, license, name, private, version, workspaces, packages/*

### Community 155 - "Agent CLI"
Cohesion: 0.33
Nodes (6): Args, main(), Option, PathBuf, Result, String

### Community 156 - "Reporter Snapshots"
Cohesion: 0.52
Nodes (6): csv_report_snapshot(), dropped_samples_mark_all_reporters_unverified(), fixture(), json_report_snapshot(), stdout_summary_snapshot(), zero_drops_are_reported_as_verified_by_all_reporters()

### Community 158 - "Wasm Differential CI"
Cohesion: 0.40
Nodes (6): F3 differential harness (native vs wasm32 runtime), TROPEL_REQUIRE_WASM gate, WASM slice job (tropel-web + size budget), tests/native_vs_wasm.rs integration test, scripts/version-lockstep.sh, scripts/wasm-size.sh

### Community 159 - "CLI Registry Build"
Cohesion: 0.40
Nodes (5): build_registry(), Option, Path, Result, String

### Community 160 - "Engine Error Types"
Cohesion: 0.40
Nodes (5): http_request_error(), Error, Error, String, TropelError

### Community 161 - "Multipart Serialization"
Cohesion: 0.47
Nodes (6): buildMultipartFormData(), bytesToBase64(), escapeMultipartFieldName(), k6FileToBytes(), serializeK6Body(), serializeUrlEncoded()

### Community 162 - "Native Bridge Functions"
Cohesion: 0.60
Nodes (5): Native Bridge Functions reference, PmBridge (__tropel_pm_* bridge functions), PmState (shared bridge state), rquickjs Func::from type constraints, tropel_native::install_all

### Community 163 - "Hawk Signing Vectors"
Cohesion: 0.50
Nodes (5): hawk_mac(), hawk_matches_api_payload_hash_vector(), hawk_matches_api_reference_vector(), hawk_normalized_orders_ts_nonce_first(), hawk_normalized_string()

### Community 164 - "Ramp-Down Claims"
Cohesion: 0.40
Nodes (3): clear_ramp_down_disables_claims(), ramp_down_claim_syncs_control_spawned(), try_claim_ramp_down_noop_when_at_or_below_target()

### Community 165 - "Agent Main Entry"
Cohesion: 0.70
Nodes (4): agent_command(), main(), outer_worker_threads(), Result

### Community 166 - "Bridge Functions Doc"
Cohesion: 0.40
Nodes (5): BRIDGE_FUNCTIONS.md, k6 API (k6-shim), Native bridge (__tropel_* host functions), k6-shim, TR-240 setup/teardown HTTP

### Community 168 - "K6 Response Shim"
Cohesion: 0.40
Nodes (3): getStatusText(), K6Response(), resolveJsonSelector()

### Community 169 - "Collection Errors"
Cohesion: 0.67
Nodes (3): CollectionError, Error, String

### Community 173 - "Task Wave Scheme"
Cohesion: 0.67
Nodes (3): TR-<wave><nn> task ID scheme, Wave dependency graph, Waves W0-W6

## Knowledge Gaps
- **316 isolated node(s):** `options`, `IdleVusGuard<'a>`, `sdk-gates.sh script`, `PredefinedVariableMeta`, `options` (+311 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **28 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Scenario` connect `Scenario Script Items` to `Engine Extension Registry`, `Stub Output`, `OpenAPI Spec Normalization`, `Input Resolution`, `SDK Test Harness`, `HAR Input Adapter`, `Postman Collection Parser`, `HTTP File Input Adapter`, `Bruno Input Adapter`, `Postman Script Runner`, `VU Loop Sampler Tasks`, `Child Process Runner`, `Insomnia Input Adapter`, `CLI Commands`, `Adapter/Driver Registry`, `Flaky Test Client`, `Registration Priority Types`, `Auth Signer Cache`, `Adapter Detect & Import`, `Stub Adapter & Driver`, `K6 Script Adapter`, `Runtime Request Execution`, `Postman Adapter Detection`?**
  _High betweenness centrality (0.131) - this node is a cross-community bridge._
- **Why does `Request` connect `Signing Schemes` to `WebSocket Protocol`, `Engine Extension Registry`, `HTTP Body & Headers Types`, `WebAssembly Host Context`, `gRPC Codec & Bounded Cache`, `HTTP Digest Authentication`, `WebSocket Integration Tests`, `Reqwest Client & Redirects`, `gRPC Streaming Integration`, `Protocol Execute`, `K6 Driver Setup & Teardown`, `HTTP Client & TLS Identity`, `Postman State & Assertions`, `Native vs Wasm Differential`, `K6 Module Export Bridge`, `Adapter/Driver Registry`, `VU Sources & Recording`, `Request Auth Header Injection`, `Auth Signer Cache`, `Postman Import Conversion`, `Stub Adapter & Driver`, `Runtime Request Execution`, `Scenario Script Items`, `K6 Request Bridge`?**
  _High betweenness centrality (0.083) - this node is a cross-community bridge._
- **Why does `JsContext` connect `JS Context & Console Shims` to `Postman Script Runner`, `K6 HTTP Bridge & Driver`, `Flaky Test Client`, `K6 Driver Bootstrap`, `Extra Functions Module`, `File Bridge & Local Imports`, `TRP Protocol & Request Vars`, `Encoding Module`, `Crypto Module (AES/HMAC)`, `JS Bootstrap & VU Context`, `JSON & Crypto Modules`, `Web JS Context Bootstrap`?**
  _High betweenness centrality (0.073) - this node is a cross-community bridge._
- **What connects `options`, `IdleVusGuard<'a>`, `sdk-gates.sh script` to the rest of the system?**
  _316 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `HTTP Client & DNS Cache` be split into smaller, more focused modules?**
  _Cohesion score 0.05083986562150056 - nodes in this community are weakly interconnected._
- **Should `K6 HTTP Bridge & Driver` be split into smaller, more focused modules?**
  _Cohesion score 0.041328236980410896 - nodes in this community are weakly interconnected._
- **Should `OAuth2 Authorization Flow` be split into smaller, more focused modules?**
  _Cohesion score 0.06105006105006105 - nodes in this community are weakly interconnected._