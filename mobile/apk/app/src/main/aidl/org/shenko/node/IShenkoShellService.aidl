package org.shenko.node;

/**
 * Shizuku UserService surface: frozen-argv execution inside the Shizuku
 * process. Shizuku transports this binder to our app; every call executes
 * our argv verbatim (no shell string is ever constructed), bounded by a
 * timeout, with bounded captured output returned as a JSON line:
 * {"exit_code":int,"stdout_b64":string,"stderr_b64":string,"timed_out":bool}
 */
interface IShenkoShellService {
    // Keep the app call at Binder FIRST_CALL_TRANSACTION (1).
    String execJson(in String[] argv, in String[] env, long timeoutMillis) = 0;

    // Shizuku API/shared 13.1.5 removes a user service with Binder code
    // 16777115 and FLAG_ONEWAY. AIDL adds FIRST_CALL_TRANSACTION to this ID.
    oneway void destroy() = 16777114;
}
