package org.plyra.glass.terminal;

/** Bounded IME composition state. Only explicit commits reach the terminal. */
final class TerminalInputState {
    static final int COMMIT = 0;
    static final int PREEDIT = 1;
    static final int KEY = 2;
    static final int IME_ENABLED = 3;
    static final int IME_DISABLED = 4;
    static final int MAX_CHARS = 4096;

    interface Sink {
        boolean send(int kind, String text, int modifiers);
    }

    private final Sink sink;
    private String composing = "";
    private int cursor;
    private int composingStart = -1;
    private int composingEnd = -1;

    TerminalInputState(Sink sink) {
        this.sink = sink;
    }

    String text() {
        return composing;
    }

    int cursor() {
        return cursor;
    }

    int composingStart() { return composingStart; }
    int composingEnd() { return composingEnd; }

    boolean setComposing(String text, int newCursorPosition) {
        if (!validText(text)) return false;
        int from = composingStart >= 0 ? composingStart : cursor;
        int to = composingEnd >= 0 ? composingEnd : cursor;
        String updated = composing.substring(0, from) + text + composing.substring(to);
        if (!validText(updated)) return false;
        if (composing.isEmpty() && !updated.isEmpty() && !sink.send(IME_ENABLED, "", 0)) {
            return false;
        }
        if (!sink.send(PREEDIT, updated, 0)) {
            return false;
        }
        composing = updated;
        composingStart = text.isEmpty() ? -1 : from;
        composingEnd = text.isEmpty() ? -1 : from + text.length();
        long position = newCursorPosition > 0
                ? (long) from + text.length() + newCursorPosition - 1 : (long) from + newCursorPosition;
        cursor = boundary(composing, (int) Math.max(0, Math.min(composing.length(), position)));
        return true;
    }

    boolean commit(String text) {
        if (!validText(text)) return false;
        int from = composingStart >= 0 ? composingStart : cursor;
        int to = composingEnd >= 0 ? composingEnd : cursor;
        return commitBuffer(composing.substring(0, from) + text + composing.substring(to));
    }

    private boolean commitBuffer(String text) {
        if (!validText(text)) return false;
        if (composing.isEmpty() && !text.isEmpty() && !sink.send(IME_ENABLED, "", 0)) {
            return false;
        }
        if (!sink.send(COMMIT, text, 0)) {
            return false;
        }
        composing = "";
        cursor = 0;
        composingStart = composingEnd = -1;
        return true;
    }

    boolean finish() {
        if (composing.isEmpty()) {
            return sink.send(PREEDIT, "", 0);
        }
        return commitBuffer(composing);
    }

    boolean replace(int start, int end, String text) {
        if (start < 0 || end < 0 || !validText(text)) return false;
        int from = boundary(composing, Math.min(Math.min(start, end), composing.length()));
        int to = Math.min(Math.max(start, end), composing.length());
        if (to > 0 && to < composing.length() && Character.isLowSurrogate(composing.charAt(to))) to++;
        // The editable contains only uncommitted composition. Replace it before
        // committing, so conversion cannot first emit the old pinyin to the PTY.
        return commitBuffer(composing.substring(0, from) + text + composing.substring(to));
    }

    boolean setComposingRegion(int start, int end) {
        int from = Math.max(0, Math.min(Math.min(start, end), composing.length()));
        int to = Math.max(0, Math.min(Math.max(start, end), composing.length()));
        if (from == to) {
            composingStart = composingEnd = -1;
            return true;
        }
        from = boundary(composing, from);
        if (to > 0 && to < composing.length() && Character.isLowSurrogate(composing.charAt(to))) to++;
        composingStart = from == to ? -1 : from;
        composingEnd = from == to ? -1 : to;
        return true;
    }

    void cancel() {
        sink.send(PREEDIT, "", 0);
        composing = "";
        cursor = 0;
        composingStart = composingEnd = -1;
    }

    boolean setSelection(int start, int end) {
        if (start != end || start < 0 || start > composing.length()) {
            return false;
        }
        cursor = boundary(composing, start);
        return true;
    }

    boolean delete(int before, int after, boolean codePoints) {
        if (before < 0 || after < 0 || before > MAX_CHARS || after > MAX_CHARS) {
            return false;
        }
        if (composing.isEmpty()) {
            for (int index = 0; index < before; index++) {
                if (!sink.send(KEY, "Backspace", 0)) return false;
            }
            for (int index = 0; index < after; index++) {
                if (!sink.send(KEY, "Delete", 0)) return false;
            }
            return true;
        }
        int start;
        int end;
        if (codePoints) {
            int availableBefore = composing.codePointCount(0, cursor);
            int availableAfter = composing.codePointCount(cursor, composing.length());
            start = composing.offsetByCodePoints(cursor, -Math.min(before, availableBefore));
            end = composing.offsetByCodePoints(cursor, Math.min(after, availableAfter));
        } else {
            start = boundary(composing, Math.max(0, cursor - before));
            end = Math.min(composing.length(), cursor + after);
            if (end > 0 && end < composing.length()
                    && Character.isLowSurrogate(composing.charAt(end))) {
                end++;
            }
        }
        String updated = composing.substring(0, start) + composing.substring(end);
        if (!sink.send(PREEDIT, updated, 0)) return false;
        if (composingStart >= 0) {
            composingStart -= Math.max(0, Math.min(end, composingStart) - start);
            composingEnd -= Math.max(0, Math.min(end, composingEnd) - start);
            if (composingStart == composingEnd) composingStart = composingEnd = -1;
        }
        composing = updated;
        cursor = start;
        return true;
    }

    boolean key(String name, int modifiers) {
        if ("Backspace".equals(name) && !composing.isEmpty()) return delete(1, 0, true);
        if ("Delete".equals(name) && !composing.isEmpty()) return delete(0, 1, true);
        if (!composing.isEmpty() && "Escape".equals(name)) {
            cancel();
            return true;
        }
        if (!composing.isEmpty() && (modifiers & 4) != 0) cancel();
        if (!composing.isEmpty()) {
            if ("Left".equals(name) && cursor > 0) {
                cursor = composing.offsetByCodePoints(cursor, -1);
                return true;
            }
            if ("Right".equals(name) && cursor < composing.length()) {
                cursor = composing.offsetByCodePoints(cursor, 1);
                return true;
            }
            if ("Home".equals(name)) {
                cursor = 0;
                return true;
            }
            if ("End".equals(name)) {
                cursor = composing.length();
                return true;
            }
            if (!"Enter".equals(name)) return true;
            if (!finish()) return false;
        }
        return sink.send(KEY, name, modifiers);
    }

    private static int boundary(String text, int position) {
        if (position > 0 && position < text.length()
                && Character.isLowSurrogate(text.charAt(position))) {
            return position - 1;
        }
        return position;
    }

    private static boolean validText(String text) {
        if (text == null || text.length() > MAX_CHARS) return false;
        for (int index = 0; index < text.length(); index++) {
            char character = text.charAt(index);
            if (Character.isHighSurrogate(character)) {
                if (++index >= text.length() || !Character.isLowSurrogate(text.charAt(index))) {
                    return false;
                }
            } else if (Character.isLowSurrogate(character)) {
                return false;
            }
        }
        return true;
    }
}

// FVPROV1 8171 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZTk2NTEyYTRlYzJjY2ViODU3NjM2Yzk2NTZmZTkwOWVmMzljYjU0MjJkZjY3YjlhMTIyM2RmMTcwYmUzMDdhYSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNmViYTQxZjU3NzM0ODlmNjU3ZjEwNWEwNWM4MWJiMDkxYWY0YmM2YmEzMmYzY2YwNzQ3N2E3YTQ4NmQ3NjM5MGIzNGJiYWJmMGFkZTI4MjQ0ODQ2ZjEyOTBiMGU1ZmZiNGM1OWJlYjUxODQxNWFhZTkxNmNhZDg4YTUzZmNiMGYifV19XQ== FVPROV1-END
