package org.shenko.node.bridge;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.ByteBuffer;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;

/** One bounded wire frame. The limit counts UTF-8 bytes, not UTF-16 characters. */
public final class BridgeFrames {
    private BridgeFrames() {}

    /**
     * Returns null for EOF before LF or an oversized frame; rejects malformed UTF-8.
     *
     * {@code cap} is the largest frame the peer may send, so exactly {@code cap}
     * bytes plus LF is a valid frame and {@code cap + 1} is not. The bound is
     * enforced while reading, so a peer that never sends LF cannot grow the
     * buffer past it.
     */
    public static String readLine(InputStream input, int cap) throws IOException {
        ByteArrayOutputStream frame = new ByteArrayOutputStream(Math.min(cap, 256));
        for (;;) {
            int next = input.read();
            if (next == -1) return null;
            if (next == '\n') {
                return StandardCharsets.UTF_8.newDecoder()
                        .onMalformedInput(CodingErrorAction.REPORT)
                        .onUnmappableCharacter(CodingErrorAction.REPORT)
                        .decode(ByteBuffer.wrap(frame.toByteArray())).toString();
            }
            frame.write(next);
            if (frame.size() > cap) return null;
        }
    }
}
