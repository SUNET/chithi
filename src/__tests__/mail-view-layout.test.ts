import { describe, expect, it } from "vitest";

import mailViewSource from "@/views/MailView.vue?raw";
import messageListSource from "@/components/mail/MessageList.vue?raw";
import messageReaderSource from "@/components/mail/MessageReader.vue?raw";

function cssRule(source: string, selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = source.match(new RegExp(`${escaped}\\s*\\{([^}]+)\\}`));
  if (!match) throw new Error(`Missing CSS rule for ${selector}`);
  return match[1];
}

describe("MailView narrow Bottom layout", () => {
  it("constrains the complete stacked reader chain to the visible width", () => {
    const stacked = cssRule(mailViewSource, ".stacked-content");
    const stackedList = cssRule(mailViewSource, ".stacked-content .message-list-pane");
    const bottomReader = cssRule(mailViewSource, ".bottom-reader-pane");
    const tabContent = cssRule(mailViewSource, ".tab-content-pane");
    const messageList = cssRule(messageListSource, ".message-list");
    const messageReader = cssRule(messageReaderSource, ".message-reader");

    for (const rule of [stacked, stackedList, bottomReader, tabContent, messageList, messageReader]) {
      expect(rule).toContain("width: 100%");
      expect(rule).toContain("min-width: 0");
    }
    expect(stacked).toContain("overflow: hidden");
    expect(bottomReader).toContain("box-sizing: border-box");
    expect(messageList).toContain("overflow-x: hidden");
  });
});
