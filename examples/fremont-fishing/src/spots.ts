export type SpotKind = "lake" | "pond" | "pier" | "warning";

export type FishingSpot = {
  id: string;
  name: string;
  area: string;
  kind: SpotKind;
  fishable: boolean;
  coordinates: [number, number];
  baselineScore: number;
  summary: string;
  bestFor: string[];
  expectedSpecies: string[];
  permitNotes: string;
  access: string;
  amenities: string[];
  cautions: string[];
  tags: string[];
  sourceNotes: string[];
  sources: { label: string; url: string }[];
};

export const sourceCatalog = [
  {
    label: "Quarry Lakes Regional Recreation Area",
    url: "https://www.ebparks.org/parks/quarry-lakes",
  },
  {
    label: "Quarry Lakes map and brochure",
    url: "https://www.ebparks.org/sites/default/files/maps/quarry-lakes-map-brochure.pdf",
  },
  {
    label: "EBRPD Angler's Edge fish plants",
    url: "https://www.ebparks.org/recreation/fishing/anglers-edge-online",
  },
  {
    label: "EBRPD lake fish list",
    url: "https://www.ebparks.org/recreation/fishing/lakes",
  },
  {
    label: "EBRPD fishing rules",
    url: "https://www.ebparks.org/recreation/fishing/rules",
  },
  {
    label: "Alameda Creek Regional Trails",
    url: "https://www.ebparks.org/trails/interpark/alameda-creek",
  },
  {
    label: "EBRPD water quality alerts",
    url: "https://www.ebparks.org/natural-resources/water-quality",
  },
  {
    label: "City of Fremont Lake Elizabeth update",
    url: "https://city.fremont.gov/lake",
  },
  {
    label: "City of Fremont Central Park",
    url: "https://www.fremont.gov/government/departments/parks-recreation/parks/central-park",
  },
  {
    label: "Don Edwards NWR fishing",
    url: "https://www.fws.gov/refuge/don-edwards-san-francisco-bay/visit-us/activities/fishing",
  },
  {
    label: "Coyote Hills map",
    url: "https://www.ebparks.org/maps/coyote-hills",
  },
];

const quarrySources = sourceCatalog.filter((source) =>
  [
    "Quarry Lakes Regional Recreation Area",
    "Quarry Lakes map and brochure",
    "EBRPD Angler's Edge fish plants",
    "EBRPD lake fish list",
    "EBRPD fishing rules",
    "EBRPD water quality alerts",
  ].includes(source.label),
);

export const fishingSpots: FishingSpot[] = [
  {
    id: "quarry-horseshoe",
    name: "Horseshoe Lake",
    area: "Quarry Lakes Regional Recreation Area",
    kind: "lake",
    fishable: true,
    coordinates: [37.5796, -121.9944],
    baselineScore: 8.9,
    summary:
      "The most complete Fremont freshwater option: stocked fish, a big-water feel, trails, picnic areas, and a real fishing pier.",
    bestFor: ["stocked trout", "catfish", "family trips", "pier access"],
    expectedSpecies: ["rainbow trout", "channel catfish", "black bass", "carp"],
    permitNotes:
      "Anglers 16+ need a California fishing license and an EBRPD Daily Fishing Access Permit.",
    access:
      "Park entry from Isherwood Way; shore access, trails, and an accessible pier. Electric boats and clean/dry car-top craft are allowed where posted.",
    amenities: ["accessible pier", "restrooms", "picnic areas", "trails", "parking", "boat launch"],
    cautions: [
      "Lead weights are not allowed.",
      "Check current blue-green algae and closure advisories before going.",
      "No water contact outside designated swim areas.",
    ],
    tags: ["fishable", "family", "pier", "stocked", "catfish", "trout", "permit"],
    sourceNotes: [
      "EBRPD lists Quarry Lakes as a water-oriented recreation area with fishing.",
      "The Quarry Lakes brochure says fishing is permitted in Horseshoe Lake and Rainbow Lake.",
      "Permit sales fund trout and catfish plants in Horseshoe Lake.",
    ],
    sources: quarrySources,
  },
  {
    id: "quarry-rainbow",
    name: "Rainbow Lake",
    area: "Quarry Lakes Regional Recreation Area",
    kind: "lake",
    fishable: true,
    coordinates: [37.5833, -121.9927],
    baselineScore: 8.1,
    summary:
      "A quieter Quarry Lakes pick when you want the same park system with less of the main-lake energy.",
    bestFor: ["shore casting", "bass", "quiet walks", "car-top craft"],
    expectedSpecies: ["rainbow trout", "black bass", "carp", "channel catfish"],
    permitNotes:
      "Anglers 16+ need a California fishing license and an EBRPD Daily Fishing Access Permit.",
    access:
      "Use Quarry Lakes trail access and posted lake edges. Car-top boats only where allowed; no trailered boats.",
    amenities: ["trails", "parking", "nearby restrooms", "picnic areas"],
    cautions: [
      "Lead weights are not allowed.",
      "Fishing is limited to posted fishable lakes in the park.",
      "Verify current rules and algae advisories.",
    ],
    tags: ["fishable", "family", "stocked", "catfish", "trout", "permit"],
    sourceNotes: [
      "EBRPD sources identify Rainbow Lake as one of the fishable Quarry Lakes waters.",
      "The broader Quarry Lakes fish list includes trout, bass, carp, and catfish.",
    ],
    sources: quarrySources,
  },
  {
    id: "shinn-pond",
    name: "Shinn Pond",
    area: "Niles / Alameda Creek Regional Trails",
    kind: "pond",
    fishable: true,
    coordinates: [37.5712, -121.9889],
    baselineScore: 7.7,
    summary:
      "The local bass-pond option: smaller, simpler, and easier to drop into than Quarry Lakes, with fewer official amenities.",
    bestFor: ["bass", "quick sessions", "no district permit", "warmwater fishing"],
    expectedSpecies: ["striped bass", "largemouth bass", "crappie", "warmwater species"],
    permitNotes:
      "Anglers 16+ need a California fishing license. EBRPD lists Shinn Pond as not requiring a District Fishing Permit.",
    access:
      "Reached from Alameda Creek Regional Trails and the Niles side of Fremont. Expect pond-style shore access.",
    amenities: ["trail access", "nearby staging areas", "bike access"],
    cautions: [
      "Swimming is never allowed at Shinn Pond.",
      "Do not fish the Alameda Creek Flood Control Channel.",
      "Amenities are more limited than at Quarry Lakes.",
    ],
    tags: ["fishable", "no-district-permit", "bass", "quick-trip"],
    sourceNotes: [
      "EBRPD lists Shinn Pond among district lakes that do not require a District Fishing Permit.",
      "CDFW Fishing in the City describes Shinn Pond as a gravel quarry pit with bass, crappie, and warmwater species.",
      "EBRPD water quality pages separately track Shinn Pond advisories.",
    ],
    sources: sourceCatalog.filter((source) =>
      [
        "EBRPD lake fish list",
        "EBRPD fishing rules",
        "Alameda Creek Regional Trails",
        "EBRPD water quality alerts",
      ].includes(source.label),
    ),
  },
  {
    id: "lake-elizabeth",
    name: "Lake Elizabeth",
    area: "Central Park",
    kind: "lake",
    fishable: true,
    coordinates: [37.5485, -121.9641],
    baselineScore: 7.1,
    summary:
      "The easiest central Fremont hangout: good paths, picnic energy, and city-park convenience, with fish stocking returning after water-quality work.",
    bestFor: ["families", "walk-and-fish", "picnics", "beginner scouting"],
    expectedSpecies: ["catfish", "trout", "carp"],
    permitNotes:
      "Verify current city rules and California license requirements before fishing.",
    access:
      "Central Park surrounds the lake with paved paths, parking areas, picnic sites, and nearby recreation facilities.",
    amenities: ["paved loop", "picnic areas", "parking", "restrooms", "snack bar seasonal", "pedal boats seasonal"],
    cautions: [
      "The city reported a July 2024 fish die-off and water-quality improvement work.",
      "Do not introduce fish, turtles, or wildlife.",
      "Verify current fishability and posted rules at the park.",
    ],
    tags: ["fishable", "family", "city-park", "trout", "catfish"],
    sourceNotes: [
      "The City of Fremont says it introduces appropriate fish such as catfish and/or trout in spring.",
      "Central Park is a 433-acre park around Lake Elizabeth with major recreation facilities.",
      "The city has public updates about water quality work after the 2024 fish die-off.",
    ],
    sources: sourceCatalog.filter((source) =>
      ["City of Fremont Lake Elizabeth update", "City of Fremont Central Park"].includes(source.label),
    ),
  },
  {
    id: "dumbarton-pier",
    name: "Dumbarton Fishing Pier",
    area: "Don Edwards San Francisco Bay National Wildlife Refuge",
    kind: "pier",
    fishable: true,
    coordinates: [37.5044, -122.1181],
    baselineScore: 8.3,
    summary:
      "The saltwater Fremont pick: a bay pier with bigger-species potential and a very different rhythm from the lakes.",
    bestFor: ["bay fishing", "pier access", "striped bass", "sharks and rays", "crabbing"],
    expectedSpecies: ["striped bass", "sculpin", "shark", "croaker", "halibut", "sturgeon", "crabs"],
    permitNotes:
      "U.S. Fish & Wildlife says a fishing license is not required for anglers using Dumbarton Fishing Pier. Verify current state and refuge rules before going.",
    access:
      "Use the Don Edwards refuge road system toward Marshlands Road and the Dumbarton Pier area.",
    amenities: ["pier", "refuge trails", "bay views", "nearby visitor center area"],
    cautions: [
      "Bay fish consumption advisories may apply.",
      "Wind, tides, mudflats, and refuge rules matter here.",
      "Check pier access and refuge notices before a trip.",
    ],
    tags: ["fishable", "pier", "bay", "saltwater", "no-license-on-pier", "striped-bass"],
    sourceNotes: [
      "Don Edwards NWR says fishing is allowed year-round and includes two bank locations and one fishing pier.",
      "The refuge page says no fishing license is required for anglers using Dumbarton Fishing Pier.",
      "The refuge lists species including striped bass, shark, croaker, halibut, sturgeon, and crabs.",
    ],
    sources: sourceCatalog.filter((source) => source.label === "Don Edwards NWR fishing"),
  },
  {
    id: "coyote-creek-warning",
    name: "Coyote Hills / Alameda Creek Channel",
    area: "Coyote Hills and Alameda Creek",
    kind: "warning",
    fishable: false,
    coordinates: [37.5564, -122.0895],
    baselineScore: 2.2,
    summary:
      "Useful to know because it looks fishy on a map, but public EBRPD materials call out no-fishing areas here.",
    bestFor: ["rule check", "avoid wrong spot", "wildlife viewing"],
    expectedSpecies: [],
    permitNotes:
      "Marked as not fishable in this app. Use nearby legal spots instead.",
    access:
      "The Coyote Hills and Alameda Creek trails are good for walking and biking, not as a fishing target in the closed areas.",
    amenities: ["trails", "wildlife viewing", "bike access"],
    cautions: [
      "EBRPD Coyote Hills map says fishing is not permitted at Coyote Hills.",
      "EBRPD Alameda Creek materials say no fishing or public access in the Alameda Creek Flood Control Channel.",
      "Use this entry as a warning, not a destination.",
    ],
    tags: ["not-fishable", "rules", "wildlife", "trail"],
    sourceNotes: [
      "Coyote Hills public map materials say fishing is not permitted.",
      "Alameda Creek Regional Trails materials warn against fishing in the flood control channel.",
    ],
    sources: sourceCatalog.filter((source) =>
      ["Coyote Hills map", "Alameda Creek Regional Trails"].includes(source.label),
    ),
  },
];

export const filterOptions = [
  { id: "fishable", label: "Fishable" },
  { id: "family", label: "Family" },
  { id: "pier", label: "Pier" },
  { id: "stocked", label: "Stocked" },
  { id: "trout", label: "Trout" },
  { id: "catfish", label: "Catfish" },
  { id: "no-district-permit", label: "No district permit" },
  { id: "bay", label: "Bay" },
  { id: "not-fishable", label: "Warnings" },
] as const;
