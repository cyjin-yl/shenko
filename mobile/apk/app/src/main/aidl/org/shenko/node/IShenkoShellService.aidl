package org.shenko.node;

import rikka.shizuku.IUserService;

/**
 * Shizuku UserService surface: frozen-argv execution inside the Shizuku
 * process. Shizuku transports this binder to our app; every call executes
 * our argv verbatim (no shell string is ever constructed), bounded by a
 * timeout, with bounded captured output returned as a JSON line:
 * {"exit_code":int,"stdout_b64":string,"stderr_b64":string,"timed_out":bool}
 */
interface IShenkoShellService extends IUserService {
    String execJson(in String[] argv, in String[] env, long timeoutMillis);
}
