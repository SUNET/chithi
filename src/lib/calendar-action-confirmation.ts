import { message as tauriMessage } from "@tauri-apps/plugin-dialog";
import type {
  CalendarActionPlan, CalendarConfirmations,
} from "./types";

/** Collect every confirmation required by a persisted calendar action plan. */
export async function confirmCalendarAction(
  plan: CalendarActionPlan,
  options: { confirmSeriesMove?: boolean } = {},
): Promise<CalendarConfirmations | null> {
  const confirmations: CalendarConfirmations = {
    replacement_meeting_identity: false,
    reset_exceptions: false,
  };
  if (options.confirmSeriesMove || plan.requires.replacement_meeting_identity) {
    const answer = await tauriMessage(
      plan.requires.replacement_meeting_identity
        ? "Moving this recurring meeting creates a replacement meeting identity in the destination calendar. Move the entire series?"
        : "Move the entire recurring series to the destination calendar?",
      {
        title: "Move Recurring Meeting",
        kind: "warning",
        buttons: { ok: "Move Series", cancel: "Cancel" },
      },
    );
    if (answer !== "Move Series" && answer !== "Ok") return null;
    confirmations.replacement_meeting_identity =
      plan.requires.replacement_meeting_identity;
  }
  if (plan.requires.reset_exceptions) {
    const answer = await tauriMessage(
      "This recurrence change removes existing exceptions. Continue?",
      {
        title: "Reset Recurring Exceptions",
        kind: "warning",
        buttons: { ok: "Reset Exceptions", cancel: "Cancel" },
      },
    );
    if (answer !== "Reset Exceptions" && answer !== "Ok") return null;
    confirmations.reset_exceptions = true;
  }
  return confirmations;
}

export async function chooseRescheduleScope(
  allowOccurrence: boolean,
): Promise<"this-occurrence" | "entire-series" | null> {
  if (!allowOccurrence) {
    const answer = await tauriMessage(
      "Reschedule the entire recurring series?",
      {
        title: "Reschedule Recurring Event",
        kind: "warning",
        buttons: { ok: "Entire Series", cancel: "Cancel" },
      },
    );
    return answer === "Entire Series" || answer === "Ok" ? "entire-series" : null;
  }
  const answer = await tauriMessage(
    "Apply this new time to only this occurrence or to the entire series?",
    {
      title: "Reschedule Recurring Event",
      kind: "info",
      buttons: {
        yes: "This Occurrence",
        no: "Entire Series",
        cancel: "Cancel",
      },
    },
  );
  if (answer === "This Occurrence" || answer === "Yes") return "this-occurrence";
  if (answer === "Entire Series" || answer === "No") return "entire-series";
  return null;
}
