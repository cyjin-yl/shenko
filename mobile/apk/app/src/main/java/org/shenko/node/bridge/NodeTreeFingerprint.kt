package org.shenko.node.bridge

import android.accessibilityservice.AccessibilityService
import android.graphics.Rect
import android.os.SystemClock
import android.view.accessibility.AccessibilityNodeInfo
import org.json.JSONArray
import org.json.JSONObject
import java.security.MessageDigest

/**
 * Deterministic fingerprint of the semantic node hierarchy the enabled
 * accessibility service can actually see (SPEC §24.5: accessibility is one
 * parallel authority, and only what it can observe is claimed).
 *
 * Honest scope, by construction:
 * - Only nodes reachable from the windows the service reports for the
 *   requested display are traversed, and only nodes the region filter selects
 *   are hashed. Both rules are part of the digest and are echoed to the node,
 *   so a digest is never compared against one taken over another scope.
 * - The traversal is bounded (nodes, depth, windows). Hitting a bound is
 *   *declared* in the answer, so the node binds a labelled prefix instead of
 *   presenting a partial walk as the whole hierarchy. A traversal that runs
 *   out of time is an error with no digest at all, never a truncated hash.
 * - Node text and content descriptions are fed into the hash and never
 *   returned: no screen content crosses the bridge, and a password field's
 *   value is never even hashed (only its presence).
 * - When the service is absent, not permitted to retrieve window content, or
 *   reports no window for the display, this throws [Unavailable] with the real
 *   reason. There is no fallback digest.
 *
 * The answer carries `algorithm`, so the node binds a digest only when it is
 * produced by the canonicalization it declares ([ALGORITHM]).
 */
object NodeTreeFingerprint {
    /** Canonicalization id; the node refuses any other value. */
    const val ALGORITHM = "shenko-node-tree-sha256/1"

    /** Inclusion rule, part of the declared scope. */
    const val NODE_FILTER = "bounds_intersect_region_or_no_area"

    private const val DIGEST_PREFIX = "sha256:"

    /** Hard ceiling regardless of what the caller asks for. */
    const val HARD_MAX_NODES = 20_000
    const val HARD_MAX_DEPTH = 128
    const val HARD_MAX_WINDOWS = 16

    /**
     * Wall-clock ceiling for one traversal. Exceeding it aborts the op with a
     * typed timeout instead of returning a digest that depends on how much of
     * the tree happened to be walked in time: a time-sliced hash would look
     * like a changed hierarchy on the next observation of an unchanged screen.
     */
    private const val DEADLINE_MS = 4_000L

    /** How often the deadline is checked while walking. */
    private const val DEADLINE_CHECK_EVERY = 32

    private const val PASSWORD_TEXT_MARKER = "\u0000password-field"

    class Unavailable(val kind: String, message: String) : Exception(message)

    private class Frame(val node: AccessibilityNodeInfo, val depth: Int, val window: Int)

    private class WindowEntry(
        val root: AccessibilityNodeInfo?,
        val packageName: String,
        val bounds: Rect,
        val type: Int,
        val sortId: Int,
        val active: Boolean,
        val focused: Boolean,
        val layer: Int,
    )

    /**
     * Fingerprint the hierarchy visible on [displayId] inside [region]
     * (display-absolute), applying exactly the requested traversal bounds and
     * echoing them back.
     */
    fun compute(
        service: AccessibilityService?,
        displayId: Int,
        region: Rect,
        maxNodes: Int,
        maxDepth: Int,
        maxWindows: Int,
    ): JSONObject {
        if (service == null) {
            throw Unavailable(
                "typed_unavailable",
                "accessibility service not enabled: no node hierarchy is observable, so no " +
                    "hierarchy fingerprint can be produced",
            )
        }
        if (!service.canRetrieveWindowContent()) {
            throw Unavailable(
                "typed_unavailable",
                "accessibility service is connected but cannot retrieve window content: the node " +
                    "hierarchy is not readable",
            )
        }
        val boundedNodes = maxNodes.coerceIn(1, HARD_MAX_NODES)
        val boundedDepth = maxDepth.coerceIn(0, HARD_MAX_DEPTH)
        val boundedWindows = maxWindows.coerceIn(1, HARD_MAX_WINDOWS)

        val startedAtMs = System.currentTimeMillis()
        val startedUptime = SystemClock.uptimeMillis()
        val deadline = startedUptime + DEADLINE_MS

        val notes = ArrayList<String>()
        val entries = windowEntries(service, displayId, notes)

        val digest = MessageDigest.getInstance("SHA-256")
        val canonical = Canonical(digest)
        canonical.write("algorithm", ALGORITHM)
        canonical.write("display_id", displayId.toString())
        canonical.write("region_left", region.left.toString())
        canonical.write("region_top", region.top.toString())
        canonical.write("region_right", region.right.toString())
        canonical.write("region_bottom", region.bottom.toString())
        canonical.write("max_nodes", boundedNodes.toString())
        canonical.write("max_depth", boundedDepth.toString())
        canonical.write("max_windows", boundedWindows.toString())
        canonical.write("node_filter", NODE_FILTER)

        val stack = ArrayDeque<Frame>()
        var windowsConsidered = 0
        var visitedNodes = 0
        var nodeCount = 0
        var truncated = false
        var truncationReason: String? = null
        val windowPackages = ArrayList<String>()

        fun truncate(reason: String) {
            if (!truncated) {
                truncated = true
                truncationReason = reason
            }
        }

        loop@ for ((ordinal, entry) in entries.withIndex()) {
            if (ordinal >= boundedWindows) {
                // Windows exist beyond the declared window bound: the digest
                // covers the ones actually entered, and says so.
                truncate("window_budget")
                break@loop
            }
            windowsConsidered++
            windowPackages.add(entry.packageName)
            canonical.write("window_index", ordinal.toString())
            canonical.write("window_package", entry.packageName)
            canonical.write("window_type", entry.type.toString())
            canonical.write("window_active", if (entry.active) "1" else "0")
            canonical.write("window_focused", if (entry.focused) "1" else "0")
            canonical.write("window_layer", entry.layer.toString())
            canonical.write("window_bounds_left", entry.bounds.left.toString())
            canonical.write("window_bounds_top", entry.bounds.top.toString())
            canonical.write("window_bounds_right", entry.bounds.right.toString())
            canonical.write("window_bounds_bottom", entry.bounds.bottom.toString())

            val root = entry.root
            if (root == null) {
                canonical.write("window_root", "unavailable")
                notes.add("window $ordinal (${entry.packageName}) reported no readable root")
                truncate("window_root_unavailable")
                continue@loop
            }
            canonical.write("window_root", "available")
            stack.addLast(Frame(root, 0, ordinal))

            while (stack.isNotEmpty()) {
                if (visitedNodes >= boundedNodes) {
                    truncate("node_budget")
                    // Release what the walk still holds instead of dropping the
                    // stack with live accessibility handles in it.
                    while (stack.isNotEmpty()) release(stack.removeLast().node)
                    stack.clear()
                    break@loop
                }
                if (visitedNodes % DEADLINE_CHECK_EVERY == 0 && SystemClock.uptimeMillis() > deadline) {
                    throw Unavailable(
                        "timeout",
                        "hierarchy traversal exceeded ${DEADLINE_MS}ms after $visitedNodes nodes; " +
                            "no digest is bound because a time-sliced prefix would depend on the " +
                            "machine, not on the screen",
                    )
                }
                val frame = stack.removeLast()
                visitedNodes++
                val node = frame.node
                val bounds = Rect()
                node.getBoundsInScreen(bounds)
                val inScope = bounds.width() <= 0 || bounds.height() <= 0 ||
                    Rect.intersects(bounds, region)

                canonical.write("node_window", frame.window.toString())
                canonical.write("node_depth", frame.depth.toString())
                canonical.write("node_class", node.className?.toString().orEmpty())
                canonical.write("node_view_id", node.viewIdResourceName.orEmpty())
                canonical.write("node_scope", if (inScope) "included" else "excluded")
                canonical.write("node_bounds_left", bounds.left.toString())
                canonical.write("node_bounds_top", bounds.top.toString())
                canonical.write("node_bounds_right", bounds.right.toString())
                canonical.write("node_bounds_bottom", bounds.bottom.toString())
                canonical.write("node_flags", flags(node))
                canonical.write("node_actions", node.actionList?.map { it.id }?.sorted()?.joinToString(",") ?: "")
                canonical.write("node_child_count", node.childCount.toString())
                // A password field's value is never hashed either: only the
                // fact that a masked value exists. Descriptions/labels stay,
                // because they are the field's identity, not its content.
                canonical.write("node_text", if (node.isPassword) PASSWORD_TEXT_MARKER else node.text?.toString().orEmpty())
                canonical.write("node_description", node.contentDescription?.toString().orEmpty())
                canonical.write("node_hint", node.hintText?.toString().orEmpty())
                if (inScope) nodeCount++

                val children = node.childCount
                if (children == 0) {
                    release(node)
                    continue
                }
                if (frame.depth >= boundedDepth) {
                    // Deeper nodes exist and were not walked: the digest stops
                    // here, and the answer must not read as the whole tree.
                    truncate("depth_budget")
                    release(node)
                    continue
                }
                var unreadableChild = false
                for (index in children - 1 downTo 0) {
                    val child = node.getChild(index)
                    if (child == null) {
                        unreadableChild = true
                        continue
                    }
                    stack.addLast(Frame(child, frame.depth + 1, frame.window))
                }
                if (unreadableChild) {
                    // A child the service would not hand over leaves a hole in
                    // the covered set; that is truncation, not completeness.
                    truncate("child_unavailable")
                }
                release(node)
            }
        }

        canonical.write("visited_nodes", visitedNodes.toString())
        canonical.write("node_count", nodeCount.toString())
        canonical.write("windows_considered", windowsConsidered.toString())
        canonical.write("truncated", if (truncated) "1" else "0")
        canonical.write("truncation_reason", truncationReason.orEmpty())
        notes.add(
            "coverage is the accessibility service's interactive window list for this display; " +
                "non-interactive surfaces and windows of other displays are not fingerprinted",
        )

        val result = JSONObject()
            .put("ok", true)
            .put("fingerprint", DIGEST_PREFIX + digest.digest().joinToString("") { "%02x".format(it) })
            .put("algorithm", ALGORITHM)
            .put("display_id", displayId)
            .put("region", JSONArray().put(region.left).put(region.top).put(region.right - region.left).put(region.bottom - region.top))
            .put("node_filter", NODE_FILTER)
            .put("max_nodes", boundedNodes)
            .put("max_depth", boundedDepth)
            .put("max_windows", boundedWindows)
            .put("windows_considered", windowsConsidered)
            .put("window_packages", JSONArray(windowPackages))
            .put("visited_nodes", visitedNodes)
            .put("node_count", nodeCount)
            .put("truncated", truncated)
            .put("sampled_at_ms", startedAtMs)
            .put("elapsed_ms", SystemClock.uptimeMillis() - startedUptime)
            .put("notes", JSONArray(notes))
        if (truncationReason != null) result.put("truncation_reason", truncationReason)
        return result
    }

    /**
     * The windows to traverse, in a structural order that does not depend on
     * window ids: two observations of an unchanged screen must traverse the
     * same order, or the digest would read as a change.
     */
    private fun windowEntries(
        service: AccessibilityService,
        displayId: Int,
        notes: MutableList<String>,
    ): List<WindowEntry> {
        val reported = try {
            service.windows
        } catch (e: Exception) {
            throw Unavailable(
                "typed_unavailable",
                "accessibility service reported no window list: ${e.message}",
            )
        }
        val entries = ArrayList<WindowEntry>()
        if (reported != null) {
            for (window in reported) {
                if (window.displayId != displayId) continue
                val root = window.root
                val bounds = Rect()
                window.getBoundsInScreen(bounds)
                entries.add(
                    WindowEntry(
                        root = root,
                        packageName = root?.packageName?.toString().orEmpty(),
                        bounds = bounds,
                        type = window.type,
                        sortId = window.id,
                        active = window.isActive,
                        focused = window.isFocused,
                        layer = window.layer,
                    ),
                )
            }
        }
        if (entries.isEmpty()) {
            // Some builds report no window list; the active window is still a
            // real, honest sample of what is on screen.
            val active = service.rootInActiveWindow
            if (active == null) {
                throw Unavailable(
                    "typed_unavailable",
                    "no window is available for display $displayId: the accessibility service " +
                        "reported no window and no active window root",
                )
            }
            val bounds = Rect()
            active.getBoundsInScreen(bounds)
            notes.add("window list was empty for this display; the active window root was used")
            entries.add(
                WindowEntry(
                    root = active,
                    packageName = active.packageName?.toString().orEmpty(),
                    bounds = bounds,
                    type = -1,
                    sortId = -1,
                    active = true,
                    focused = true,
                    layer = -1,
                ),
            )
        }
        // Structural tie-breaks first and the (volatile) window id last, so an
        // unchanged screen keeps its traversal order across observations.
        return entries.sortedWith(
            compareBy(
                { it.packageName },
                { it.bounds.top },
                { it.bounds.left },
                { it.type },
                { -it.layer },
                { if (it.active) 0 else 1 },
                { if (it.focused) 0 else 1 },
                { it.sortId },
            ),
        )
    }

    /**
     * The node's structural state as a fixed-length bit string. Only
     * deterministic, non-volatile properties are included: caret positions,
     * extras and timestamps would make an unchanged screen hash differently.
     */
    @Suppress("DEPRECATION")
    private fun release(node: AccessibilityNodeInfo) {
        // `recycle()` is a no-op from API 33 on and still frees the native
        // handle on older releases this app supports (minSdk 26). A throwing
        // implementation must not abort an otherwise valid walk.
        runCatching { node.recycle() }
    }

    private fun flags(node: AccessibilityNodeInfo): String = buildString(32) {
        append(if (node.isClickable) '1' else '0')
        append(if (node.isLongClickable) '1' else '0')
        append(if (node.isScrollable) '1' else '0')
        append(if (node.isFocusable) '1' else '0')
        append(if (node.isFocused) '1' else '0')
        append(if (node.isEditable) '1' else '0')
        append(if (node.isCheckable) '1' else '0')
        append(if (node.isChecked) '1' else '0')
        append(if (node.isEnabled) '1' else '0')
        append(if (node.isSelected) '1' else '0')
        append(if (node.isVisibleToUser) '1' else '0')
        append(if (node.isPassword) '1' else '0')
        append(if (node.isMultiLine) '1' else '0')
        append(if (node.isContentInvalid) '1' else '0')
    }

    /**
     * Length-delimited canonical writer, so no two different field sequences
     * can produce the same byte stream ("a" + "bc" vs "ab" + "c").
     */
    private class Canonical(private val digest: MessageDigest) {
        fun write(tag: String, value: String) {
            digest.update(tag.toByteArray(Charsets.UTF_8))
            val bytes = value.toByteArray(Charsets.UTF_8)
            digest.update(
                byteArrayOf(
                    (bytes.size ushr 24).toByte(),
                    (bytes.size ushr 16).toByte(),
                    (bytes.size ushr 8).toByte(),
                    bytes.size.toByte(),
                ),
            )
            digest.update(bytes)
        }
    }
}
