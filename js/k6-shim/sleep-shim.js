// ══════════════════════════════════════════════════════════════════
// k6 sleep() shim — delegates to the native __tropel_native_sleep
// bridge (installed per-VU by the K6DriverInstance / engine). The
// bridge blocks the OS thread, which is safe under thread-per-core
// (1 VU per dedicated worker thread).
// ══════════════════════════════════════════════════════════════════
if (typeof sleep === 'undefined') {
  function sleep(seconds) {
    if (typeof __tropel_native_sleep === 'function') {
      // TR-245: pump modern WebSocket events before/around the blocking
      // sleep so a server-pushed message that arrives during sleep() fires
      // the onmessage/addEventListener handlers (k6's event loop does this;
      // tropel has no async event loop, so the pump is the bridge). The
      // deferred-modules shim defines this; guarded here because sleep-shim
      // is bundled BEFORE it.
      if (typeof __tropel_websocket_pump_all === 'function') {
        __tropel_websocket_pump_all();
      }
      __tropel_native_sleep(seconds * 1000);
      if (typeof __tropel_websocket_pump_all === 'function') {
        __tropel_websocket_pump_all();
      }
    }
  }
}
