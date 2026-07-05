import type { FishingSpot } from "../types";

const sources = {
  quarry: "https://www.ebparks.org/parks/quarry-lakes",
  quarryMap: "https://www.ebparks.org/sites/default/files/maps/quarry-lakes-map-brochure.pdf",
  fishLakes: "https://www.ebparks.org/recreation/fishing/lakes",
  rules: "https://www.ebparks.org/recreation/fishing/rules",
  alamedaTrail: "https://www.ebparks.org/trails/interpark/alameda-creek",
  alamedaMap: "https://www.ebparks.org/sites/default/files/maps/AlamedaCreekTrails-MapBrochure.pdf",
  water: "https://www.ebparks.org/natural-resources/water-quality",
  cdfw: "https://wildlife.ca.gov/Fishing-in-the-City/SF/Gofish/Southeast",
  niles: "https://www.fremont.gov/Home/Components/FacilityDirectory/FacilityDirectory/88/822",
  snellProject:
    "https://www.fremont.gov/government/departments/parks-planning-design/design-projects/niles-community-park-erosion-control",
  lakeUpdate: "https://city.fremont.gov/lake",
  centralPark: "https://www.fremont.gov/government/departments/parks-recreation/parks/central-park",
  donEdwards:
    "https://www.fws.gov/refuge/don-edwards-san-francisco-bay/visit-us/activities/fishing",
  coyote: "https://www.ebparks.org/maps/coyote-hills",
};

export const spots: FishingSpot[] = [
  {
    id: "lake-elizabeth",
    name: "Lake Elizabeth",
    area: "Central Park, Fremont",
    coordinates: { lat: 37.5489, lng: -121.9651 },
    status: "fishable",
    summary:
      "The easiest Fremont city-lake option: paved loop, family amenities, and warmwater fishing with stocking history, but water quality has needed close attention.",
    species: ["largemouth bass", "sunfish", "catfish", "trout", "carp present"],
    permitNotes:
      "Public source notes say no fishing or parking fees are charged. California license rules may still apply by age and method; verify before fishing.",
    accessNotes:
      "Shoreline access around the Central Park lake with nearby parking, picnic areas, playgrounds, and a two-mile loop trail.",
    amenities: ["restrooms nearby", "picnic areas", "playgrounds", "walking loop", "snack bar seasonally"],
    cautions: [
      "City updates describe a 2024 fish die-off tied to heat and low dissolved oxygen.",
      "Verify posted water-quality signs and current stocking before planning around trout or catfish.",
    ],
    publicSourceNotes: [
      "CDFW identifies Lake Elizabeth as a 63-acre Fremont Central Park reservoir with largemouth bass and sunfish and periodic trout/catfish stocking.",
      "City of Fremont describes Lake Elizabeth as an 83-acre man-made lake and says spring fish introductions depend on water-quality measures.",
      "Central Park lists fishing among park amenities.",
    ],
    sources: [
      { label: "CDFW Southeast Bay public fishing locations", url: sources.cdfw },
      { label: "City Lake Elizabeth update", url: sources.lakeUpdate },
      { label: "City Central Park", url: sources.centralPark },
    ],
    tags: ["fishable", "family", "stocked", "no-district-permit", "warnings"],
    baselineScore: 82,
  },
  {
    id: "quarry-horseshoe",
    name: "Quarry Lakes - Horseshoe Lake",
    area: "Quarry Lakes Regional Recreation Area",
    coordinates: { lat: 37.5777, lng: -121.9872 },
    status: "fishable",
    summary:
      "Best all-around Fremont choice for a planned freshwater session, with stocked-lake species, a daily district permit, and an accessible fishing pier.",
    species: ["rainbow trout", "largemouth bass", "smallmouth bass", "bluegill", "channel catfish"],
    permitNotes:
      "Anglers 16+ need a California fishing license and a daily District Fishing Permit at Quarry Lakes.",
    accessNotes:
      "Use the Isherwood or Niles-side park access; EBRPD notes an ADA fishing pier at Horseshoe Lake.",
    amenities: ["ADA fishing pier", "parking", "restrooms", "picnic areas", "trails", "boat launch nearby"],
    cautions: [
      "Fishing at Quarry Lakes is limited to Horseshoe Lake and Rainbow Lake.",
      "Check algae, mussel, fish-plant, and closure notices before going.",
    ],
    publicSourceNotes: [
      "EBRPD lists Quarry Lakes fishing only at Horseshoe and Rainbow Lakes.",
      "EBRPD lists Quarry Lakes species including trout, bass, bluegill/sunfish, and channel catfish.",
      "The Quarry Lakes page references a daily fishing permit and accessible Horseshoe Lake fishing pier.",
    ],
    sources: [
      { label: "Quarry Lakes park page", url: sources.quarry },
      { label: "Quarry Lakes map/brochure", url: sources.quarryMap },
      { label: "EBRPD fish by lake", url: sources.fishLakes },
      { label: "EBRPD fishing rules", url: sources.rules },
    ],
    tags: ["fishable", "family", "pier", "stocked", "warnings"],
    baselineScore: 90,
  },
  {
    id: "quarry-rainbow",
    name: "Quarry Lakes - Rainbow Lake",
    area: "Quarry Lakes Regional Recreation Area",
    coordinates: { lat: 37.5807, lng: -121.9854 },
    status: "fishable",
    summary:
      "A second legal Quarry Lakes target with the same lake complex species, useful when Horseshoe is crowded or wind direction favors the east bank.",
    species: ["rainbow trout", "largemouth bass", "smallmouth bass", "bluegill", "channel catfish"],
    permitNotes:
      "Anglers 16+ need a California fishing license and a daily District Fishing Permit at Quarry Lakes.",
    accessNotes:
      "Reach from Quarry Lakes trails and parking areas; stay out of Lago Los Osos and Willow Slough for fishing or water contact.",
    amenities: ["parking", "restrooms", "trails", "picnic areas"],
    cautions: [
      "EBRPD has posted blue-green algae danger advisories for Rainbow Lake in public water-quality notices; verify current status.",
      "No fishing or water contact in Lago Los Osos or Willow Slough.",
    ],
    publicSourceNotes: [
      "EBRPD identifies Rainbow Lake as one of the two fishable Quarry Lakes waters.",
      "The Quarry Lakes page points anglers to Angler's Edge for fish planting schedules.",
    ],
    sources: [
      { label: "Quarry Lakes park page", url: sources.quarry },
      { label: "EBRPD fish by lake", url: sources.fishLakes },
      { label: "EBRPD water quality alerts", url: sources.water },
      { label: "EBRPD fishing rules", url: sources.rules },
    ],
    tags: ["fishable", "family", "stocked", "warnings"],
    baselineScore: 78,
  },
  {
    id: "shinn-pond",
    name: "Shinn Pond",
    area: "Niles/Fremont, Alameda Creek Trails",
    coordinates: { lat: 37.5772, lng: -121.9745 },
    status: "fishable",
    summary:
      "A small, separate Niles pond recognized by EBRPD and CDFW, useful for quick bank fishing without the Quarry Lakes district permit.",
    species: ["rainbow trout", "largemouth bass", "bluegill", "black crappie", "channel catfish", "white catfish"],
    permitNotes:
      "EBRPD identifies Shinn Pond as not requiring a District Fishing Permit; anglers 16+ still need a California fishing license.",
    accessNotes:
      "Use Alameda Creek Trail/Niles access. This entry is for Shinn Pond, not Snell Pond and not the Alameda Creek flood-control channel.",
    amenities: ["trail access", "nearby street access", "bike access"],
    cautions: [
      "Swimming is never allowed at Shinn Pond.",
      "Do not treat adjacent Alameda Creek channel sections as open fishing access without posted confirmation.",
    ],
    publicSourceNotes: [
      "EBRPD lists Shinn Pond separately from Quarry Lakes and says it does not require a District Fishing Permit.",
      "EBRPD fish table lists trout, bass, bluegill/sunfish, crappie, channel catfish, and white catfish for Shinn Pond.",
      "Alameda Creek Trails water-quality notes identify Shinn Pond and prohibit swimming there.",
    ],
    sources: [
      { label: "EBRPD fish by lake", url: sources.fishLakes },
      { label: "EBRPD fishing rules", url: sources.rules },
      { label: "Alameda Creek Regional Trails", url: sources.alamedaTrail },
      { label: "CDFW Southeast Bay public fishing locations", url: sources.cdfw },
    ],
    tags: ["fishable", "stocked", "no-district-permit", "warnings"],
    baselineScore: 74,
  },
  {
    id: "snell-pond",
    name: "Snell Pond at Niles Community Park",
    area: "Niles Community Park, Fremont",
    coordinates: { lat: 37.5779, lng: -121.9787 },
    status: "verify",
    summary:
      "A Niles neighborhood pond next to park and erosion-control project references. Treat it as a separate place from Shinn Pond and verify fishing permission on-site.",
    species: ["unsourced local pond species - verify"],
    permitNotes:
      "Fishing permission is rule-verification-needed. Do not assume the Shinn Pond permit note applies to Snell Pond.",
    accessNotes:
      "Use Niles Community Park access and posted city signs. The City project page confirms Snell Pond as a separate park pond context.",
    amenities: ["park setting", "nearby paths", "neighborhood access"],
    cautions: [
      "No public source in this app confirms current fishing permission at Snell Pond.",
      "Verify posted signs, City of Fremont rules, and any project closures before fishing.",
    ],
    publicSourceNotes: [
      "City sources identify Niles Community Park and a Snell Pond erosion-control project.",
      "This app intentionally keeps Snell Pond separate from Shinn Pond because EBRPD/CDFW fishing notes apply to Shinn Pond, not Snell Pond.",
    ],
    sources: [
      { label: "City Niles Community Park", url: sources.niles },
      { label: "City Snell Pond erosion-control project", url: sources.snellProject },
    ],
    tags: ["warnings"],
    baselineScore: 45,
  },
  {
    id: "niles-canyon-alameda-creek",
    name: "Niles Canyon / Alameda Creek",
    area: "Mouth of Niles Canyon and Alameda Creek corridor",
    coordinates: { lat: 37.5799, lng: -121.9799 },
    status: "verify",
    summary:
      "Scenic creek-corridor water near Niles, but fishing access is not the same as Shinn Pond. Confirm legal access and rules before casting.",
    species: ["creek species not confirmed for public harvest in app sources"],
    permitNotes:
      "Rule-verification-needed. EBRPD license rules name Alameda Creek Trail for Shinn Pond; this is not blanket permission for channel fishing.",
    accessNotes:
      "Alameda Creek Regional Trail follows the creek from the mouth of Niles Canyon toward the Bay. Stay on public trail access and respect closures.",
    amenities: ["regional trail", "bike access", "walking access"],
    cautions: [
      "Alameda Creek flood-control channel sections are not presented here as public fishing access.",
      "A 2026 sediment project may close portions of the Isherwood staging area and trail; verify current closures.",
    ],
    publicSourceNotes: [
      "EBRPD describes Alameda Creek Regional Trail as following Alameda Creek from the mouth of Niles Canyon westward.",
      "EBRPD notes temporary 2026 trail/staging closures for sediment removal between Isherwood and I Street.",
      "This is separate from Shinn Pond and from Snell Pond.",
    ],
    sources: [
      { label: "Alameda Creek Regional Trails", url: sources.alamedaTrail },
      { label: "Alameda Creek map/brochure", url: sources.alamedaMap },
      { label: "EBRPD fishing rules", url: sources.rules },
    ],
    tags: ["warnings"],
    baselineScore: 38,
  },
  {
    id: "dumbarton-pier",
    name: "Dumbarton Fishing Pier",
    area: "Don Edwards San Francisco Bay NWR",
    coordinates: { lat: 37.5059, lng: -122.1195 },
    status: "fishable",
    summary:
      "The strongest saltwater option near Fremont: year-round pier fishing for Bay species, with no license required when fishing from the pier.",
    species: ["striped bass", "sculpin", "shark", "croaker", "halibut", "sturgeon", "crabs"],
    permitNotes:
      "FWS says a fishing license is not required for anglers using Dumbarton Fishing Pier. Follow state and refuge rules.",
    accessNotes:
      "Access the pier from the Newark/Fremont side of Highway 84 near the Bay and Don Edwards refuge lands.",
    amenities: ["pier", "bay shoreline", "parking nearby", "wildlife refuge setting"],
    cautions: [
      "Check refuge rules, tides, wind, and fish-consumption advisories before going.",
      "Bay mud, currents, and weather can change quickly.",
    ],
    publicSourceNotes: [
      "FWS says fishing is allowed year-round at Don Edwards refuge fishing locations.",
      "FWS lists Dumbarton Fishing Pier species and says no license is required for anglers on that pier.",
      "CDFW lists Old Dumbarton Bridge Pier as a public pier location reached from Highway 84 west from Newark.",
    ],
    sources: [
      { label: "Don Edwards NWR fishing", url: sources.donEdwards },
      { label: "CDFW Southeast Bay public fishing locations", url: sources.cdfw },
    ],
    tags: ["fishable", "pier", "no-district-permit", "bay", "warnings"],
    baselineScore: 86,
  },
  {
    id: "coyote-hills-warning",
    name: "Coyote Hills / Alameda Creek Channel Warning",
    area: "Coyote Hills Regional Park and south-bank channel area",
    coordinates: { lat: 37.553, lng: -122.091 },
    status: "not-fishable",
    summary:
      "Keep this on the list as a warning: Coyote Hills is a great park, but the public map states fishing is not permitted.",
    species: [],
    permitNotes:
      "Not fishable in this app. Do not use Alameda Creek channel proximity as permission to fish inside Coyote Hills.",
    accessNotes:
      "Use the park and Alameda Creek Trail for hiking, biking, birding, and shoreline views, not fishing.",
    amenities: ["trails", "visitor center nearby", "bay views", "wildlife viewing"],
    cautions: [
      "EBRPD Coyote Hills map says fishing is not permitted.",
      "Respect marsh closures and posted park rules.",
    ],
    publicSourceNotes: [
      "EBRPD's Coyote Hills map explicitly marks fishing as not permitted.",
      "Alameda Creek Trail provides access to Coyote Hills, but that does not override Coyote Hills park rules.",
    ],
    sources: [
      { label: "Coyote Hills map", url: sources.coyote },
      { label: "Alameda Creek Regional Trails", url: sources.alamedaTrail },
    ],
    tags: ["not-fishable", "warnings"],
    baselineScore: 5,
  },
];
