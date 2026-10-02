"""Map every actual payload owner and static contributor to original terms."""
import json
from pathlib import Path
import re


def registry(providers, owners, qt, static, digest):
    components = {}
    app = json.loads((providers['app'] / 'manifest.json').read_text())
    runtime = json.loads((providers['runtime'] / 'manifest.json').read_text())
    ubuntu = json.loads((providers['stage'] / 'ubuntu-sources.json').read_text())
    package_components = {}
    external_notices = []

    def add(identifier, terms, evidence, **extra):
        if identifier not in components:
            components[identifier] = {'component_id': identifier, 'license_expression': terms,
                                      'license_evidence': evidence, **extra}
        return identifier

    authored = add('furami', 'MIT', 'usr/share/licenses/furami/LICENSE',
                   source_refs=['furami/source-snapshot'], role='authored-source')
    app_components = [authored]
    for crate in app['crates']:
        if crate['name'] == 'furami' or crate['role'] == 'not-selected':
            continue
        app_components.append(add(f'cargo:{crate["name"]}@{crate["version"]}', crate.get('license_expression'),
                                  {'cargo_lock_checksum': crate['checksum'], 'vendor': crate['vendor_path']},
                                  role=crate['role'], source_refs=[crate['vendor_path']]))
    for item in app['binary']['rust_standard_library']['archives']:
        app_components.append(add('rust-std:' + item['name'], item['license_expression'],
                                  {'linked_archive_sha256': item['sha256'], 'source': item['source_path']},
                                  role='static-linked-input', source_refs=[app['binary']['rust_standard_library']['source']]))
    for item in app['binary']['direct_objects']:
        app_components.append(add('app-CRT:' + item['binary_package']['name'], item['license_expression'],
                                  item['notice'], role='retained-static-object', source_refs=[item['source_package']],
                                  license_exception=item.get('license_exception')))
    for record in ubuntu['packages']:
        notice = providers['stage'] / 'ubuntu-source-evidence' / record['notice']['path']
        if notice.is_symlink() or digest(notice) != record['notice']['sha256']:
            raise ValueError(f'Ubuntu component notice changed: {record["binary_package"]}')
        identifier = add('ubuntu:' + record['binary_package'] + '@' + record['binary_version'],
                         record['applicable_license_labels'] or None,
                         {'notice_sha256': record['notice']['sha256'], 'notice': record['notice']['path'],
                          'kind': 'original Debian copyright labels, not inferred SPDX',
                          'sections': record['license_sections']}, source_refs=record['files'],
                         source_required=record['source_required'], source_package=record['source_package'],
                         version=record['source_version'])
        components[identifier]['terms_selection_resolved'] = record['terms_selection_resolved']
        package_components[record['binary_package']] = identifier
        external_notices.append(notice)
    runtime_components = []
    for item in runtime['components']:
        runtime_components.append(add('runtime:' + item['name'], item['spdx'],
                                      {'notices': {name: runtime['notice_files'][name] for name in item['notice_files']},
                                       'linked_archives': runtime['linked_archives'], 'linked_objects': runtime['linked_objects']},
                                      source_refs=[{'url': item['source_url'], 'sha256': item['source_sha256']}],
                                      role='outer-runtime-static'))
    static_components = [package_components[item['package']] for item in static['components']]
    mapped_owners = {}
    for path, owner in owners.items():
        label = owner['owner']
        if owner.get('components'):
            identifiers = []
            for item in owner['components']:
                base = item['component_id']
                identifier = base + '@' + owner['source'] if base.startswith('qt:') else base
                identifier = add(identifier, item.get('license_expression'), item['evidence'],
                                 file_spdx_id=item.get('file_spdx_id'), source_component_id=base,
                                 role=item.get('role', 'runtime-file'))
                identifiers.append(identifier)
            if 'libplacebo' in identifiers:
                identifiers += static_components
        elif label == 'app':
            identifiers = app_components
        elif label.startswith('ubuntu:'):
            package = label.split(':', 1)[1].split('=', 1)[0]
            if package not in package_components:
                raise ValueError(f'Ubuntu owner missing source/notice identity: {label}')
            identifiers = [package_components[package]]
        elif label == 'source-built-runtime':
            identifiers = runtime_components
        elif label.startswith(('app-crate-source', 'KDAB-cxx-qt')):
            identifiers = app_components
        elif label.startswith('rust-std'):
            identifiers = [identifier for identifier in app_components if identifier.startswith('rust-std:')]
        elif label == 'checked-source-notice':
            name = owner['source'].split('.', 1)[0]
            terms = {'mpv': 'GPL-2.0-or-later', 'ffmpeg': 'GPL-2.0-or-later', 'libplacebo': 'LGPL-2.1-or-later'}.get(name)
            identifiers = [add(name, terms, owner['source'])]
        elif label == 'checked-ICU-notice':
            item = next(item for other in owners.values() for item in other.get('components', [])
                        if item['component_id'] == 'icu:73.2')
            identifiers = [add('icu:73.2', item['license_expression'], item['evidence'])]
        elif label in ('qualification-template', 'qualification-generated'):
            identifiers = [authored]
        elif label == 'ubuntu-license-text':
            # License text is legal material, not a falsely MIT-attributed code component.
            identifiers = [add('license-text:' + Path(path).name, None, owner['source'], role='verbatim-license-notice')]
        else:
            raise ValueError(f'no actual component provenance: {path}: {label}')
        mapped_owners[path] = [components[identifier] for identifier in dict.fromkeys(identifiers)]
        if not mapped_owners[path]:
            raise ValueError(f'empty actual component mapping: {path}')
    for path, mapped in mapped_owners.items():
        owners[path]['components'] = mapped
    # Follow runtime module DEPENDS_ON only. Root CONTAINS would include unshipped
    # build tools and styles, falsely broadening distribution obligations.
    for document in qt:
        packages = {item['SPDXID']: item for item in document['packages']}
        texts = {item['licenseId']: item.get('extractedText') for item in document['extracted_license_texts']}
        selected_sources = {owner['source'] for path, owner in owners.items()
                            if owner['owner'] == 'media' and not path.startswith('usr/share/')}
        original = json.loads((providers['media'] / document['path']).read_text())
        selected_files = {file['SPDXID'] for file in original['files']
                          if file['fileName'].removeprefix('./') in selected_sources}
        queue = [row['spdxElementId'] for row in document['relationships']
                 if row['relationshipType'] == 'CONTAINS' and row['relatedSpdxElement'] in selected_files]
        visited = set()
        incorporated = []
        while queue:
            identifier = queue.pop()
            if identifier in visited:
                continue
            visited.add(identifier)
            package = packages.get(identifier)
            if package and '-qt-3rdparty-' in identifier and '-system-' not in identifier:
                incorporated.append(add('qt-static:' + identifier, package.get('licenseConcluded'),
                                        {'SPDX_document': document['path'], 'package': package,
                                         'custom_license_texts': texts}, role='Qt-incorporated-source',
                                        source_refs=[package.get('downloadLocation')]))
            queue.extend(row['relatedSpdxElement'] for row in document['relationships']
                         if row['spdxElementId'] == identifier and row['relationshipType'] == 'DEPENDS_ON')
        module = 'qt:' + document['path'].split('/')[-1].split('-6.11.2')[0]
        for path, owner in owners.items():
            if owner['owner'] == 'media' and not path.startswith('usr/share/') and any(
                    item.get('source_component_id') == module for item in owner['components']):
                owner['components'].extend(components[identifier] for identifier in incorporated)
    for identifier, record in components.items():
        expression = record['license_expression']
        legal_material = record['role'] in ('verbatim-license-notice', 'vendor-license-evidence') if 'role' in record else False
        # Published Qt public module alternatives permit LGPLv3 selection without
        # selecting Qt Commercial or GPLv2-only. No custom terms are invented.
        selected = 'LGPL-3.0-only' if isinstance(expression, str) and 'LGPL-3.0-only' in expression.split(' OR ') else expression
        custom = re.findall(r'LicenseRef-[A-Za-z0-9.-]+', selected or '') if isinstance(selected, str) else []
        evidence = record['license_evidence']
        available_custom = evidence.get('custom_license_texts', {}) if isinstance(evidence, dict) else {}
        record['selected_terms'] = selected
        record['terms_resolved'] = legal_material or bool(selected and selected not in ('NOASSERTION', 'NONE') and
                                                         all(available_custom.get(name) for name in custom) and
                                                         record.get('terms_selection_resolved', True))
        if legal_material:
            record['terms_basis'] = 'verbatim legal/configuration material, not executable code; original terms retained'
    return components, runtime_components, external_notices
