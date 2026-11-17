package org.plyra.glass.terminal;

import java.util.ArrayList;
import java.util.List;

public final class TerminalInputStateTest {
    private static int checks;

    private static void check(boolean result, String message) {
        checks++;
        if (!result) throw new AssertionError(message);
    }

    public static void main(String[] args) {
        List<String> events = new ArrayList<>();
        TerminalInputState state = new TerminalInputState((kind, text, modifiers) -> {
            events.add(kind + ":" + text + ":" + modifiers);
            return true;
        });
        check(state.setComposing("zhong", 1), "pinyin preedit accepted");
        check(state.setComposing("中文", 1), "candidate preedit accepted");
        check(events.stream().noneMatch(event -> event.startsWith("0:")), "preedit never commits");
        check(state.commit("中文"), "Chinese commit accepted");
        check(state.finish(), "finish after commit accepted");
        check(events.stream().filter(event -> event.equals("0:中文:0")).count() == 1,
                "candidate committed exactly once");
        check(state.text().isEmpty(), "committed state cleared");
        events.clear();
        state.setComposing("nihao", 1);
        check(state.replace(0, 5, "你好"), "candidate replaces the uncommitted pinyin");
        check(events.stream().filter(event -> event.startsWith("0:")).count() == 1
                && events.contains("0:你好:0"), "replacement commits Chinese once without old pinyin");
        check(!state.replace(-1, 0, "x"), "negative replacement bounds rejected");
        check(events.get(0).equals("3::0"), "new composition enables the Rust IME transaction");

        check(state.setComposing("A😀中", 1), "supplementary preedit accepted");
        check(state.delete(1, 0, false) && state.text().equals("A😀"), "UTF16 delete Chinese");
        check(state.delete(1, 0, false) && state.text().equals("A"), "UTF16 delete preserves pair");
        check(state.setComposing("A😀中", 1), "reset preedit");
        int previousEvents = events.size();
        check(state.key("Left", 0) && state.cursor() == 3 && events.size() == previousEvents,
                "composition arrow changes local cursor without committing");
        check(state.setSelection(1, 1), "move cursor before emoji");
        check(state.delete(0, 1, true) && state.text().equals("A中"), "codepoint forward delete");
        check(state.finish() && events.contains("0:A中:0"), "finish commits remaining preedit");

        events.clear();
        check(state.delete(2, 1, true), "terminal deletion accepted");
        check(events.size() == 3 && events.get(0).equals("2:Backspace:0")
                && events.get(2).equals("2:Delete:0"), "deletion emits terminal keys");
        check(!state.delete(-1, 0, true), "negative deletion rejected");
        check(!state.setComposing("\ud800", 1), "unpaired surrogate rejected");
        check(!state.commit(new String(new char[4097])), "oversized input rejected");
        state.setComposing("未提交", 1);
        events.clear();
        check(state.key("c", 4), "Ctrl+C accepted during composition");
        check(events.equals(java.util.Arrays.asList("1::0", "2:c:4")),
                "Ctrl+C cancels preedit without committing");
        events.clear();
        check(state.commit("续") && events.equals(java.util.Arrays.asList("3::0", "0:续:0")),
                "fresh input enables IME again after cancellation");
        TerminalInputState unavailable = new TerminalInputState((kind, text, modifiers) -> false);
        check(!unavailable.setComposing("中文", 1) && unavailable.text().isEmpty(),
                "failed delivery leaves state unchanged");
        state.setComposing("abcdef", 1);
        check(state.setComposingRegion(4, 2) && state.setComposing("中", 1)
                && state.text().equals("ab中ef") && state.cursor() == 3
                && state.composingStart() == 2 && state.composingEnd() == 3,
                "partial reversed composing region preserves its prefix and suffix");
        events.clear();
        check(state.commit("文") && events.equals(java.util.Arrays.asList("0:ab文ef:0")),
                "partial candidate commits the complete buffered text exactly once");
        state.setComposing("abcdef", 1);
        state.setComposingRegion(2, 4);
        events.clear();
        check(state.finish() && events.equals(java.util.Arrays.asList("0:abcdef:0")),
                "finish preserves the complete buffer without duplicating a partial region");
        char[] maximum = new char[TerminalInputState.MAX_CHARS];
        java.util.Arrays.fill(maximum, 'a');
        String preserved = new String(maximum);
        state.setComposing(preserved, 1);
        state.setComposingRegion(1, 2);
        events.clear();
        check(!state.setComposing("xx", 1) && !state.commit("xx") && !state.replace(1, 2, "xx")
                && state.text().equals(preserved) && state.composingStart() == 1
                && state.composingEnd() == 2 && events.isEmpty(),
                "oversized combined replacements leave buffered text and region untouched");
        state.cancel();
        state.setComposing("A😀B", 1);
        check(state.setComposingRegion(2, 3) && state.setComposing("中", 1)
                && state.text().equals("A中B"), "partial region cannot split a surrogate pair");
        state.setSelection(1, 1);
        check(state.delete(0, 1, true) && state.text().equals("AB")
                && state.composingStart() == -1 && state.composingEnd() == -1,
                "deleting a partial composition adjusts and clears its region");
        System.out.println("TerminalInputState " + checks + " checks passed");
    }
}

// FVPROV1 5766 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZmZlMzkwNWQ2N2NmZDNhZWNkYTQ1ZGE1ZmFhOTI4NmQ3MDNmMjhjYTM4NDkzZDAxZDJkNTBkNjE0MzJjMWZiYyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMDU0YjQzODg4MGYxZTk4NTU0OGU2ZGJkMTc5OWNkNjQwNGJmNDE4MDAyZWI1NmQ1Nzc1ZDA2YjI0NWMyZmI4MDc4MmJjZDkxNzIxNzZlMDc1M2QxNGFlMzNmMTRkY2I0NGUxODhlZDRiZGJmZWFlNjViYjJjNjhhNjQ0MWViMGMifV19XQ== FVPROV1-END
