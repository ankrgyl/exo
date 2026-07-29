import { bootstrapProfile } from "./bootstrap";
import { practicalProfile } from "./practical";
import type { ExoProfile, ExoProfileName } from "./types";

const PROFILES: Record<ExoProfileName, ExoProfile> = {
  bootstrap: bootstrapProfile,
  practical: practicalProfile,
};

export function resolveExoProfile(name = process.env.EXO_PROFILE): ExoProfile {
  const profileName = name ?? "practical";
  if (profileName !== "bootstrap" && profileName !== "practical") {
    throw new Error(
      `unknown EXO_PROFILE ${JSON.stringify(profileName)}; expected bootstrap or practical`,
    );
  }
  return PROFILES[profileName];
}

export type { ExoProfile, ExoProfileName } from "./types";
