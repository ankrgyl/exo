import { bootstrapProfile } from "./bootstrap";
import { offlineProfile } from "./offline";
import { practicalProfile } from "./practical";
import type { ExoProfile, ExoProfileName } from "./types";

const PROFILES: Record<ExoProfileName, ExoProfile> = {
  bootstrap: bootstrapProfile,
  practical: practicalProfile,
  offline: offlineProfile,
};

export function resolveExoProfile(name = process.env.EXO_PROFILE): ExoProfile {
  const profileName = name ?? "practical";
  if (!isProfileName(profileName)) {
    throw new Error(
      `unknown EXO_PROFILE ${JSON.stringify(profileName)}; expected ${Object.keys(PROFILES).join(", ")}`,
    );
  }
  return PROFILES[profileName];
}

function isProfileName(name: string): name is ExoProfileName {
  return name in PROFILES;
}

export type { ExoProfile, ExoProfileName } from "./types";
