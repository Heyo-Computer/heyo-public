export type FishableStatus = "fishable" | "verify" | "not-fishable";

export type FilterTag =
  | "fishable"
  | "not-fishable"
  | "family"
  | "pier"
  | "stocked"
  | "no-district-permit"
  | "bay"
  | "warnings";

export type FishingSpot = {
  id: string;
  name: string;
  area: string;
  coordinates: { lat: number; lng: number };
  status: FishableStatus;
  summary: string;
  species: string[];
  permitNotes: string;
  accessNotes: string;
  amenities: string[];
  cautions: string[];
  publicSourceNotes: string[];
  sources: { label: string; url: string }[];
  tags: FilterTag[];
  baselineScore: number;
};

export type CatchReport = {
  id: string;
  spotId: string;
  rating: number;
  species: string;
  bait: string;
  date: string;
  crowd: "Quiet" | "Moderate" | "Busy";
  note: string;
  createdAt: string;
};
