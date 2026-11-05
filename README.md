# Aptimizer Studio

**AptCAD and AptBIM in one window.** A bar at the top switches between the two modes:

- **AptCAD**: drafting. Drawings, layers, dimensions, hatches, blocks, layouts and plots; opens and saves DXF and DWG.
- **AptBIM**: building modelling. Levels, walls, doors and windows, rooms, structure and services, sections, sheets and
  schedules; opens AptBIM projects, IFC and Revit (`.rvt`) files.

Each mode keeps its work while the other is shown: a drawing open in AptCAD is still there after a visit to AptBIM, and
the other way round. It runs on Linux, macOS and Windows.

*By Aptimizer.*

## Run it

You need [Rust](https://rustup.rs) 1.95 or newer and git with access to
[Anuj09k/AptBIM-](https://github.com/Anuj09k/AptBIM-), which is private (cargo fetches it with your git's own
credentials; see `.cargo/config.toml`).

```sh
git clone https://github.com/Anuj09k/Aptimizer-Studio
cd Aptimizer-Studio
cargo run                      # development build, starts in AptBIM
cargo run -- --cad             # start in AptCAD
cargo run -- plan.dwg          # open a drawing in AptCAD
cargo run -- house.aptbim      # open a project in AptBIM
cargo run --release            # a fast build, as you'd ship it
```

The first build downloads and compiles both apps and takes a while (about 10 to 20 minutes); later ones are quick.

On Linux, install the system libraries first (Debian/Ubuntu names):

```sh
sudo apt-get install -y libxkbcommon-dev libwayland-dev libfontconfig1-dev libgtk-3-dev
```

`aptimizer-studio --help` lists the options. A drawing (`.dxf`, `.dwg`, `.dwt`, `.svg`) opens in AptCAD, a project
(`.aptbim`, `.ifc`, `.rvt`) in AptBIM; the window starts in the mode of the last file given, or the one `--cad` / `--bim`
asks for, or AptBIM.

## How it fits together

Both apps are libraries with a thin window around them: AptCAD's `CadApp` and AptBIM's `BimApp`. This program
(`src/main.rs`) owns one of each and shows one at a time below the mode bar. They share the window's drawing context,
and each sets its fonts and colours to its own, so switching modes asks the incoming app to apply its look again
(`restyle`, which each app provides for this).

`Cargo.toml` pins each app to a commit of its repository. To take newer versions, change the `rev` of the
`aptcad-*` or `aptbim-*` dependencies and run `cargo update`. Both apps must stay on the same egui: the
`[patch.crates-io]` section repeats AptCAD's, and AptBIM builds on the same fork.

The window has no stencil buffer: AptBIM's plan canvas is made for a render pass without one, and AptCAD then clips
layout viewports on the processor, as its web build does.

## Builds

`.github/workflows/build.yml` tests and builds the app on Linux, macOS and Windows on every push and keeps each build as
a workflow artifact. It needs a repository secret, `APTBIM_TOKEN`: a GitHub token that can read
`Anuj09k/AptBIM-` (see the top of the workflow).

## Not yet

- One installer and an updater for the studio (each app has its own; this program uses neither yet).
- Closing the window from one mode doesn't ask about unsaved work in the other: save in both before quitting.
- AptCAD's macOS system menu bar: in the studio its menus stay inside the window.
- Opening an AptBIM view in AptCAD, and linking an AptCAD drawing into AptBIM, without going through files.

## License

Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)), because AptBIM's Revit reader is Apache-2.0 only. The notices of AptCAD,
AptBIM and the code they build on are in [NOTICE](NOTICE). The AptCAD and AptBIM names and logos are Aptimizer's and are
not covered by the license.
