import { json } from "@codemirror/lang-json";
import { yaml } from "@codemirror/lang-yaml";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { HighlightStyle, StreamLanguage, syntaxHighlighting } from "@codemirror/language";
import { EditorState } from "@codemirror/state";
import { drawSelection, EditorView, highlightActiveLine, keymap, lineNumbers } from "@codemirror/view";
import { tags } from "@lezer/highlight";
import { useEffect, useRef } from "react";

const env = StreamLanguage.define<{ value: boolean }>({
  startState: () => ({ value: false }),
  token(stream, state) {
    if (stream.sol()) {
      state.value = false;
      if (stream.match(/^\s*#.*/)) return "comment";
      if (stream.match(/^\s*\[[^\]]*\]\s*$/)) return "heading";
      if (stream.match(/^\s*secret\b/)) return "keyword";
    }
    if (state.value) {
      stream.skipToEnd();
      return "string";
    }
    if (stream.eatSpace()) return null;
    if (stream.eat("=")) {
      state.value = true;
      return "operator";
    }
    stream.eatWhile(/[^=\s]/);
    return "variableName";
  },
});

const look = HighlightStyle.define([
  { tag: tags.comment, color: "var(--ink-3)" },
  { tag: tags.heading, color: "var(--ink)", fontWeight: "600" },
  { tag: tags.keyword, color: "var(--warn)" },
  { tag: tags.variableName, color: "var(--ink)", fontWeight: "500" },
  { tag: tags.operator, color: "var(--ink-3)" },
  { tag: tags.string, color: "var(--ink-2)" },
  { tag: [tags.propertyName, tags.definition(tags.propertyName)], color: "var(--ink)", fontWeight: "500" },
  { tag: [tags.number, tags.bool, tags.null, tags.atom], color: "var(--ok)" },
  { tag: [tags.content, tags.attributeValue], color: "var(--ink-2)" },
  { tag: tags.punctuation, color: "var(--ink-3)" },
]);

const theme = EditorView.theme({
  "&": { background: "var(--well)", color: "var(--ink)", fontSize: "0.8rem" },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": { fontFamily: "var(--font-mono)", lineHeight: "1.65", minHeight: "9rem" },
  ".cm-content": { caretColor: "var(--ink)", padding: "0.5rem 0" },
  ".cm-cursor": { borderLeftColor: "var(--ink)" },
  ".cm-gutters": { background: "var(--well)", color: "var(--ink-3)", border: "none", borderRight: "1px solid var(--rule)" },
  ".cm-activeLine": { background: "color-mix(in oklch, var(--ink) 6%, transparent)" },
  ".cm-selectionBackground, &.cm-focused .cm-selectionBackground, ::selection": { background: "color-mix(in oklch, var(--ink) 25%, transparent) !important" },
});

/** A text editor in the console's own colours. */
export default function Editor({ value, onChange, label, language = "env", fill }: { value: string; onChange?: (text: string) => void; label: string; language?: "env" | "yaml" | "json"; fill?: boolean }) {
  const host = useRef<HTMLDivElement>(null);
  const change = useRef(onChange);
  change.current = onChange;
  useEffect(() => {
    if (!host.current) return;
    const view = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: value,
        extensions: [
          lineNumbers(),
          history(),
          drawSelection(),
          highlightActiveLine(),
          keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
          language === "yaml" ? yaml() : language === "json" ? json() : env,
          EditorState.readOnly.of(!onChange),
          EditorView.editable.of(!!onChange),
          fill ? EditorView.theme({ "&": { height: "100%" }, ".cm-scroller": { overflow: "auto" } }) : EditorView.theme({ ".cm-scroller": { maxHeight: "60dvh", overflow: "auto" } }),
          syntaxHighlighting(look),
          theme,
          EditorView.contentAttributes.of({ "aria-label": label, spellcheck: "false", autocapitalize: "off", autocorrect: "off" }),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) change.current?.(u.state.doc.toString());
          }),
        ],
      }),
    });
    return () => view.destroy();
    // The editor owns the text while it's open; a new `value` comes with a new key.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return <div ref={host} className={`border border-rule ${fill ? "h-full" : ""}`} />;
}
