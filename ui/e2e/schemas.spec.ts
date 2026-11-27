import { expect, test } from "@playwright/test";
import { mutationSchema } from "../src/lib/schemas";

test("observation timestamps accept whole microseconds within the API's i64 range", () => {
  const accepts = (occurredAtUs: number) => mutationSchema.safeParse({
    operation: "addObservation",
    payload: { entityName: "event", body: "note", occurredAtUs },
  }).success;

  expect(accepts(2 ** 53)).toBe(true);
  expect(accepts(0.5)).toBe(false);
  expect(accepts(-1)).toBe(false);
});
