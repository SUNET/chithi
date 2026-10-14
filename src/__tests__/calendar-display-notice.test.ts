import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import CalendarDisplayNotice from "@/components/calendar/CalendarDisplayNotice.vue";

describe("Calendar display notice", () => {
  it("has one collapsed notice regardless of resource count, with all details retained", () => {
    const errors = Array.from({ length: 40 }, (_, index) => `Remote resource ${index} could not be displayed`);
    const wrapper = mount(CalendarDisplayNotice, { props: { errors } });
    const notice = wrapper.get("details");
    expect(notice.attributes("role")).toBe("alert");
    expect(notice.attributes("open")).toBeUndefined();
    expect(wrapper.findAll(".calendar-incomplete")).toHaveLength(1);
    expect(wrapper.get("summary").text()).toContain("40 issue(s)");
    expect(wrapper.findAll("li")).toHaveLength(40);
    expect(wrapper.findAll("li")[39].text()).toBe(errors[39]);
  });

  it("removes the notice after a complete successful read", async () => {
    const wrapper = mount(CalendarDisplayNotice, { props: { errors: ["Sign-in expired"] } });
    expect(wrapper.get("summary").text()).toContain("1 issue(s)");
    await wrapper.setProps({ errors: [] });
    expect(wrapper.find("details").exists()).toBe(false);
  });
});
