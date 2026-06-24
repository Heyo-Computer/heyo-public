# Fremont Fishing Holes

Build a deployable web app for discovering, comparing, and rating fishing spots in and near Fremont, California. The product should feel like a compact location-ranking app, similar in spirit to Nomads.com, but focused on local fishing holes: practical rankings, filters, local notes, and source-backed spot details.

Use Vite + React + TypeScript unless the project already has a stronger convention. Keep v1 local-first: seed public data in code and store community ratings/reports in browser `localStorage`. Do not add auth, a database, paid APIs, maps API keys, scraping infrastructure, or a backend service.

The app must be deployable through Heyo/heyvm. `npm start` must serve the production build on port `3000`.

## Source Data

Seed public-source facts from these pages. Do not imply real-time conditions. Label sourced claims as public source notes, and tell users to verify current rules, closures, licenses, water quality, fish consumption advisories, and posted signs before going.

- Quarry Lakes Regional Recreation Area: https://www.ebparks.org/parks/quarry-lakes
- Quarry Lakes map/brochure: https://www.ebparks.org/sites/default/files/maps/quarry-lakes-map-brochure.pdf
- EBRPD Angler's Edge fish plants: https://www.ebparks.org/recreation/fishing/anglers-edge-online
- EBRPD "Which Fish Are in Which Lakes?": https://www.ebparks.org/recreation/fishing/lakes
- EBRPD fishing rules: https://www.ebparks.org/recreation/fishing/rules
- Alameda Creek Regional Trails: https://www.ebparks.org/trails/interpark/alameda-creek
- Alameda Creek Regional Trails map/brochure: https://www.ebparks.org/sites/default/files/maps/AlamedaCreekTrails-MapBrochure.pdf
- EBRPD water quality alerts: https://www.ebparks.org/natural-resources/water-quality
- CDFW Fishing in the City, Southeast Bay public fishing locations: https://wildlife.ca.gov/Fishing-in-the-City/SF/Gofish/Southeast
- City of Fremont Niles Community Park: https://www.fremont.gov/Home/Components/FacilityDirectory/FacilityDirectory/88/822
- City of Fremont Niles Community Park / Snell Pond erosion project: https://www.fremont.gov/government/departments/parks-planning-design/design-projects/niles-community-park-erosion-control
- City of Fremont Lake Elizabeth update: https://city.fremont.gov/lake
- City of Fremont Central Park: https://www.fremont.gov/government/departments/parks-recreation/parks/central-park
- Don Edwards NWR fishing: https://www.fws.gov/refuge/don-edwards-san-francisco-bay/visit-us/activities/fishing
- Coyote Hills map note that fishing is not permitted: https://www.ebparks.org/maps/coyote-hills

## Seed Spots

Include these entries:

- Lake Elizabeth
- Quarry Lakes - Horseshoe Lake
- Quarry Lakes - Rainbow Lake
- Shinn Pond, a separate Niles/Fremont fishing pond that CDFW and EBRPD identify separately from the Alameda Creek flood control channel
- Snell Pond at Niles Community Park, a separate pond from Shinn Pond; mark fishing permission as rule-verification-needed unless a public source clearly confirms current fishing permission
- Niles Canyon / Alameda Creek, a separate creek/canyon area entry; mark as rule-verification-needed and warn that Alameda Creek flood control channel sections are not public fishing access
- Dumbarton Fishing Pier
- Coyote Hills / Alameda Creek channel warning entry, marked as not fishable

Each spot should include:

- Name, area, coordinates, and fishable/not-fishable status
- Short local-style summary
- Expected species where sourced
- Permit/license notes
- Access notes
- Amenities
- Cautions
- Source URLs
- Tags used by filters
- Baseline score used before user ratings exist

## Tasks

- [ ] Scaffold the deployable frontend app
  Create a Vite + React + TypeScript project in this directory. Add `npm run dev`, `npm run build`, `npm run preview`, and `npm start`. `npm start` must run a production preview server on `0.0.0.0:3000` for Heyo.

- [ ] Add seeded Fremont fishing spot data
  Put typed seed data in a dedicated module. Include the eight required spot entries and source metadata. Keep source facts auditable by linking every spot to its public sources. Treat Shinn Pond, Snell Pond at Niles Community Park, and Niles Canyon / Alameda Creek as three separate places. For Snell Pond and Niles Canyon / Alameda Creek, distinguish confirmed location/access/habitat facts from fishing-permission claims that users must verify.

- [ ] Build the ranked directory UI
  The first screen must be the usable app, not a marketing landing page. Show ranked spot cards, compact stats, search, and filter chips. Include filters for fishable/not fishable, family friendly, pier access, stocked trout/catfish, no district permit, bay/saltwater, and warnings.

- [ ] Add spot detail view
  Let users select a spot and see practical details: expected species, permit notes, access, amenities, cautions, coordinates, source notes, and source links. Include a simple built-in visual map or location graphic without relying on external map APIs.

- [ ] Add user ratings and catch reports
  Let users rate a spot from 1-5 and submit a catch/report note with species, bait, date, crowd level, and free-form note. Store reports in `localStorage`. Blend user ratings with the baseline score so rankings update after reports are added.

- [ ] Add comparison and advisory panels
  Add a side-by-side comparison surface for top spots covering permits, species, access, amenities, and cautions. Add a visible but compact advisory telling users to verify current rules, closures, licenses, water quality, and consumption advisories.

- [ ] Polish responsive app styling
  Make the app scan well on desktop and mobile. Use stable card dimensions, compact controls, readable typography, and a restrained multi-color palette. Avoid oversized hero sections, marketing copy, decorative gradient blobs, and nested cards.

- [ ] Verify locally
  Run `npm install`, `npm run build`, and `npm start`. Confirm `curl -I http://localhost:3000/` returns `200 OK`. Run `npm audit --audit-level=moderate` and report any remaining issues.

- [ ] Verify Heyo run path
  Confirm the app can run inside a Heyo sandbox with Node 22 using a command equivalent to:
  `heyvm create --name fremont-fishing --image node:22 --needs-network --no-ttl --mount "$PWD:/workspace" --start-command "cd /workspace && npm install && npm run build && npm start" --open-port 3004:3000`.
  Confirm `curl -I http://localhost:3004/` returns `200 OK`.

## Acceptance Criteria

- The app builds with TypeScript strict mode.
- The UI works without a backend or network-only runtime dependency.
- User ratings and reports persist after page refresh.
- Every seeded fishing spot has visible source links.
- Shinn Pond, Snell Pond at Niles Community Park, and Niles Canyon / Alameda Creek are separate entries with clear notes explaining how they differ in the Niles area.
- The Coyote Hills / Alameda Creek entry is clearly marked as a warning/not-fishable entry.
- The production server works on port `3000`.
- The Heyo sandbox run path is documented by command output or notes in the final response.
