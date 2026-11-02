package org.shenko.node;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.util.concurrent.TimeUnit;

/** Shared root/Shizuku/Dhizuku capture. Both pipes are drained, even after the retention cap. */
public final class BoundedProcessCapture {
    public static final int OUTPUT_CAP = 4 * 1024 * 1024;

    public static final class Result {
        public final int exitCode;
        public final byte[] stdout;
        public final byte[] stderr;
        public final boolean timedOut;

        private Result(int exitCode, byte[] stdout, byte[] stderr, boolean timedOut) {
            this.exitCode = exitCode;
            this.stdout = stdout;
            this.stderr = stderr;
            this.timedOut = timedOut;
        }
    }

    private BoundedProcessCapture() {}

    public static Result capture(Process process, long timeoutMillis)
            throws IOException, InterruptedException {
        Drain stdout = new Drain(process.getInputStream());
        Drain stderr = new Drain(process.getErrorStream());
        Thread outThread = new Thread(stdout, "shenko-shell-stdout");
        Thread errThread = new Thread(stderr, "shenko-shell-stderr");
        outThread.start();
        errThread.start();
        try {
            // This is finite argv execution, not an interactive shell.
            process.getOutputStream().close();
            boolean timedOut = !process.waitFor(timeoutMillis, TimeUnit.MILLISECONDS);
            if (timedOut) {
                process.destroyForcibly();
                if (!process.waitFor(1, TimeUnit.SECONDS)) {
                    throw new IOException("shell did not terminate after its deadline");
                }
            }
            outThread.join(1000);
            errThread.join(1000);
            if (outThread.isAlive() || errThread.isAlive()) {
                throw new IOException("shell output pipes did not reach EOF");
            }
            if (stdout.failure != null) throw stdout.failure;
            if (stderr.failure != null) throw stderr.failure;
            return new Result(timedOut ? -1 : process.exitValue(),
                    stdout.bytes.toByteArray(), stderr.bytes.toByteArray(), timedOut);
        } finally {
            if (process.isAlive()) process.destroyForcibly();
            // Closing the owned streams releases readers on every failure path.
            // A cleanup failure must not replace the primary capture failure.
            try { process.getInputStream().close(); } catch (IOException ignored) {}
            try { process.getErrorStream().close(); } catch (IOException ignored) {}
            try { process.getOutputStream().close(); } catch (IOException ignored) {}
            outThread.interrupt();
            errThread.interrupt();
            outThread.join();
            errThread.join();
        }
    }

    private static final class Drain implements Runnable {
        private final InputStream input;
        private final ByteArrayOutputStream bytes = new ByteArrayOutputStream(8192);
        private IOException failure;

        private Drain(InputStream input) { this.input = input; }

        @Override
        public void run() {
            byte[] buffer = new byte[16 * 1024];
            try {
                int count;
                while ((count = input.read(buffer)) != -1) {
                    int keep = Math.min(count, OUTPUT_CAP - bytes.size());
                    if (keep > 0) bytes.write(buffer, 0, keep);
                    // The remaining bytes are discarded, not left in the pipe.
                }
            } catch (IOException error) {
                failure = error;
            }
        }
    }
}
