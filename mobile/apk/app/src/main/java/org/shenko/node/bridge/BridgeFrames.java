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

    /** Returns null for EOF before LF or an oversized frame; rejects malformed UTF-8. */
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
            if (frame.size() == cap) return null;
            frame.write(next);
        }
    }
}
