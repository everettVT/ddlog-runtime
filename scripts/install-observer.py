#!/usr/bin/env python3
"""Install opt-in native logging into a pinned generated DDlog project.

Validate every patch site before any write. A changed generator fails closed.
The installed code is dormant unless DDLOG_OBSERVER_FILE is set at execution.

When the program imports the built-in star library, its generated copy
(`types/lemmalog_star/lemmalog_star.rs`) is also patched with the three named
phase regions (`large-star`, `small-star`, `minimum-label`). That is build-time
observation support: it changes the native binary, never the registered
definition or its content hash. `observer-install.json` records what was done.
"""
from pathlib import Path
import argparse
import json

ROOT = Path(__file__).resolve().parent.parent
WORKER_SITE = '            self.worker.log_register().remove("differential/arrange");\n        };\n        Ok(())'
WORKER_PATCH = WORKER_SITE.replace('        Ok(())', '        crate::observer::install(self.worker)?;\n        Ok(())')
CLI_SITE = '        differential_idle_merge_effort: args.idle_merge_effort,'
CLI_PATCH = CLI_SITE + '\n        enable_debug_regions: std::env::var_os("DDLOG_OBSERVER_FILE").is_some(),'
STAR_LIBRARY = Path('types/lemmalog_star/lemmalog_star.rs')
# Pinned to src/star/lemmalog_star.rs; the patched text equals the reference
# observer implementation (observer/examples/connected_components/lemmalog_star.rs).
STAR_SITES = [
    ('large-star open',
     '    let stars = pairs.filter(|(u, v)| u != v).iterate(|current| {\n'
     '        let symmetric = current.concat(&current.map(|(u, v)| (v, u)));\n'
     '        let large = symmetric\n',
     '    let stars = pairs.filter(|(u, v)| u != v).iterate(|current| {\n'
     '        let large = current.scope().region_named("large-star", |region| {\n'
     '        let current = current.enter_region(region);\n'
     '        let symmetric = current.concat(&current.map(|(u, v)| (v, u)));\n'
     '        symmetric\n'),
    ('large-star close / small-star open',
     '            .map(|(_, edge)| edge)\n'
     '            .distinct_core::<Weight>();\n'
     '        // Every large-star edge already points from larger to smaller.\n'
     '        // Keep the orientation explicit to document the small-star map phase.\n'
     '        large\n'
     '            .map(|(u, v)| if u > v { (u, v) } else { (v, u) })\n',
     '            .map(|(_, edge)| edge)\n'
     '            .distinct_core::<Weight>().leave_region()\n'
     '        });\n'
     '        // Every large-star edge already points from larger to smaller.\n'
     '        // Keep the orientation explicit to document the small-star map phase.\n'
     '        large.scope().region_named("small-star", |region| {\n'
     '        large.enter_region(region)\n'
     '            .map(|(u, v)| if u > v { (u, v) } else { (v, u) })\n'),
    ('small-star close / minimum-label',
     '            // must dissipate, otherwise an empty logical delta can circulate.\n'
     '            .distinct_core::<Weight>()\n'
     '    });\n'
     '    stars\n'
     '        .concat(&nodes.map(|node| (node, node)))\n'
     '        .reduce(|_, labels, output| output.push((*labels[0].0, 1)))\n'
     '        .map(move |(node, label)| pack_label(ddlog_std::tuple2(node, label)))\n'
     '}',
     '            // must dissipate, otherwise an empty logical delta can circulate.\n'
     '            .distinct_core::<Weight>().leave_region()\n'
     '        })\n'
     '    });\n'
     '    stars.scope().region_named("minimum-label", |region| {\n'
     '    let nodes = nodes.enter_region(region);\n'
     '    stars.enter_region(region)\n'
     '        .concat(&nodes.map(|node| (node, node)))\n'
     '        .reduce(|_, labels, output| output.push((*labels[0].0, 1)))\n'
     '        .map(move |(node, label)| pack_label(ddlog_std::tuple2(node, label))).leave_region()\n'
     '    })\n'
     '}'),
]


def patch_once(source, original, patched, label):
    # Worker replacement removes the original; CLI replacement contains it.
    expected_original_count = patched.count(original)
    if source.count(patched) == 1 and source.count(original) == expected_original_count:
        return source
    if patched in source or source.count(original) != 1:
        raise ValueError(f"Pinned {label} hook site changed or duplicated")
    return source.replace(original, patched, 1)


def patch_star(source):
    """Wrap the three star phases in named regions; every site must pin exactly."""
    for label, original, patched in STAR_SITES:
        source = patch_once(source, original, patched, f'star {label}')
    return source


def install(project):
    project = Path(project)
    lib = project / 'differential_datalog/src/lib.rs'
    worker = project / 'differential_datalog/src/program/worker.rs'
    cli = project / 'src/main.rs'
    source = lib.read_text()
    if source.count('mod observer;') > 1:
        raise ValueError('Duplicate observer module declaration')
    module = source if 'mod observer;' in source else source + '\nmod observer;\n'
    worker_text = patch_once(worker.read_text(), WORKER_SITE, WORKER_PATCH, 'worker')
    cli_text = patch_once(cli.read_text(), CLI_SITE, CLI_PATCH, 'native CLI')
    native_text = (ROOT / 'native/observer.rs').read_text()
    writes = [(lib, module), (worker, worker_text), (cli, cli_text),
              (project / 'differential_datalog/src/observer.rs', native_text)]
    star = project / STAR_LIBRARY
    star_phases = None
    if star.exists():
        writes.append((star, patch_star(star.read_text())))
        star_phases = True
    for path, text in writes:
        if not path.exists() or path.read_text() != text:
            path.write_text(text)
    marker = {'schema_version': 1, 'observer_hook': True, 'star_phases': star_phases,
              'phases': ['large-star', 'small-star', 'minimum-label'] if star_phases else []}
    (project / 'observer-install.json').write_text(json.dumps(marker, indent=1) + '\n')
    return marker


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('project', type=Path)
    install(parser.parse_args().project)
