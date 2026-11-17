package org.plyra.glass.terminal;

import java.util.ArrayList;
import java.util.List;

public final class TerminalTouchStateTest {
    private static int checks;

    private static void check(boolean result, String message) {
        checks++;
        if (!result) throw new AssertionError(message);
    }

    public static void main(String[] args) {
        List<String> pointers = new ArrayList<>();
        List<Double> deltas = new ArrayList<>();
        TerminalTouchState state = new TerminalTouchState(new TerminalTouchState.Sink() {
            public boolean pointer(int phase, double x, double y) {
                pointers.add(phase + ":" + x + ":" + y);
                return true;
            }
            public boolean scroll(double x, double y, double dx, double dy) {
                deltas.add(dy);
                return true;
            }
        });
        check(state.event(0, 7, 10, 20), "primary pointer accepted");
        check(state.event(1, 9, 90, 90) && pointers.size() == 1,
                "unknown pointer cannot move primary capture");
        check(state.event(1, 7, 15, 25), "captured pointer moves");
        check(state.event(2, 7, 20, 30), "primary pointer ends");
        check(pointers.equals(java.util.Arrays.asList("0:10.0:20.0", "1:15.0:25.0", "2:20.0:30.0")),
                "tap/drag preserves its complete pointer lifecycle");

        pointers.clear();
        check(state.event(0, 1, 10, 10) && state.event(0, 2, 20, 10), "two pointers accepted");
        check(pointers.equals(java.util.Arrays.asList("0:10.0:10.0", "3:10.0:10.0")),
                "second finger cancels selection without a click release");
        check(state.event(1, 1, 10, 20) && state.event(1, 2, 20, 20), "two finger movement accepted");
        check(deltas.size() == 2 && deltas.get(0) + deltas.get(1) == 10,
                "scroll uses centroid displacement exactly once");
        check(state.event(2, 1, 10, 20) && state.event(1, 2, 20, 30)
                && state.event(2, 2, 20, 30) && pointers.size() == 2,
                "remaining finger does not become an accidental click");
        check(state.event(0, 3, 1, 2), "new gesture after scrolling accepted");
        state.cancel();
        check(pointers.get(pointers.size() - 1).equals("3:1.0:2.0"), "pause cancels captured pointer");
        check(!state.event(0, 1, Double.NaN, 1), "nonfinite coordinates rejected");
        TerminalTouchState unavailable = new TerminalTouchState(new TerminalTouchState.Sink() {
            public boolean pointer(int phase, double x, double y) { return false; }
            public boolean scroll(double x, double y, double dx, double dy) { return false; }
        });
        check(!unavailable.event(0, 1, 1, 1), "failed delivery cannot establish pointer capture");
        pointers.clear();
        check(!state.longPress(), "idle cannot open commands");
        state.event(0, 1, 12, 34);
        check(state.longPress(), "single finger long press opens commands");
        state.event(2, 1, 12, 34);
        check(pointers.equals(java.util.Arrays.asList("0:12.0:34.0", "3:12.0:34.0")),
                "long press cancels capture and its eventual up cannot click");
        check(!state.longPress(), "long press fires once per gesture");
        state.event(0, 1, 1, 1);
        state.event(0, 2, 2, 2);
        check(!state.longPress(), "two finger scrolling cannot open commands");
        state.cancel();
        System.out.println("TerminalTouchState " + checks + " checks passed");
    }
}

// FVPROV1 3529 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiM2JiMjliNWQyNTY3NjczMDRhZWUzNzk5MDliZWE3NTRhMmEyNjI3YTExMmE1OGFkYjE1MjI5Yzg5NjcxNGVkNCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYWI0ZGNkOTg3ZmUyMTRjNjRiZDI0M2ExZDJlYjUzNDFjZDY4YzFiN2YyMDliNDA0YTRjMmIxZThiYTI5ZDkzZGU4YmVjNjc5Y2Q5YTU1MjI5NGZkYWFiMzBiMjA5OGRjNjEzYTMwY2FmZmYxN2U2M2E2ZmYxN2ZlZTcwOTFkMDkifV19XQ== FVPROV1-END
