package org.plyra.glass.terminal;

/** One-finger pointer capture and bounded two-finger scrolling. */
final class TerminalTouchState {
    static final int STARTED = 0;
    static final int MOVED = 1;
    static final int ENDED = 2;
    static final int CANCELLED = 3;

    interface Sink {
        boolean pointer(int phase, double x, double y);
        boolean scroll(double x, double y, double dx, double dy);
    }

    private final Sink sink;
    private int primary = -1;
    private int secondary = -1;
    private double x1, y1, x2, y2;
    private boolean scrolling;

    TerminalTouchState(Sink sink) {
        this.sink = sink;
    }

    boolean event(int phase, int id, double x, double y) {
        if (id < 0 || !Double.isFinite(x) || !Double.isFinite(y)) return false;
        if (phase == STARTED) {
            if (primary < 0 && secondary < 0) {
                if (!sink.pointer(STARTED, x, y)) return false;
                primary = id;
                x1 = x;
                y1 = y;
                scrolling = false;
            } else if (id != primary && secondary < 0 && !scrolling) {
                sink.pointer(CANCELLED, x1, y1);
                secondary = id;
                x2 = x;
                y2 = y;
                scrolling = true;
            }
            return true;
        }
        if (id != primary && id != secondary) return true;
        double previousX = (x1 + x2) * 0.5;
        double previousY = (y1 + y2) * 0.5;
        if (id == primary) { x1 = x; y1 = y; }
        else { x2 = x; y2 = y; }
        if (phase == MOVED) {
            if (!scrolling) return sink.pointer(MOVED, x, y);
            if (primary >= 0 && secondary >= 0) {
                double centerX = (x1 + x2) * 0.5;
                double centerY = (y1 + y2) * 0.5;
                return sink.scroll(centerX, centerY, centerX - previousX, centerY - previousY);
            }
            return true;
        }
        if (phase != ENDED && phase != CANCELLED) return false;
        boolean result = scrolling || sink.pointer(phase, x, y);
        if (id == primary) primary = -1;
        else secondary = -1;
        if (primary < 0 && secondary < 0) scrolling = false;
        return result;
    }

    boolean longPress() {
        if (primary < 0 || secondary >= 0 || scrolling) return false;
        cancel();
        return true;
    }

    void cancel() {
        if (primary >= 0 && !scrolling) sink.pointer(CANCELLED, x1, y1);
        primary = secondary = -1;
        scrolling = false;
    }
}

// FVPROV1 2535 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYjA0MDJjMWE3ZmI0YjhiMDE2NGI4Y2RkNTUxZjE4NjFiOTAxZTJmZGJiYmM2ZWU2NjZmNDdhMDA0MmU4ZmYzMSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMTI2MDc1OWIzOTQ0ZmExM2ZhYjhiYjI5YmFkYmQ0M2E2NTVhZGVkMjNkNTg5ZjEyODY2YTczZjZjN2Y3ODBmODhjMzQ1YjhiNTdmNDAwOGNjMTVjODAwYjg0ODY3YTJjNzhiY2YyODc4MDA4ZmY5MmRiMzYxNTI2ZmYzYWIxMDEifV19XQ== FVPROV1-END
