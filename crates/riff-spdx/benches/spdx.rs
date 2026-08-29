use criterion::{black_box, criterion_group, criterion_main, Criterion};
use riff_spdx::SpdxLicenses;

const EXPRESSIONS: [&str; 10] = [
    "MIT",
    "Apache-2.0",
    "(MIT OR GPL-3.0-only)",
    "GPL-2.0-or-later WITH Classpath-exception-2.0",
    "BSD-3-Clause AND MIT",
    "(LGPL-2.1-only OR BSD-3-Clause) AND MIT",
    "proprietary",
    "MIT AND (Apache-2.0 OR ISC)",
    "EUPL-1.2",
    "not-a-license",
];

fn bench_new(c: &mut Criterion) {
    c.bench_function("spdx_load_licenses", |b| {
        b.iter(|| black_box(SpdxLicenses::new()))
    });
}

fn bench_validate(c: &mut Criterion) {
    let licenses = SpdxLicenses::new();

    c.bench_function("spdx_validate_expressions", |b| {
        b.iter(|| {
            for expression in EXPRESSIONS {
                black_box(licenses.validate(black_box(expression)));
            }
        })
    });
}

fn bench_lookup(c: &mut Criterion) {
    let licenses = SpdxLicenses::new();
    let identifiers = ["MIT", "apache-2.0", "GPL-3.0-only", "BSD-3-Clause", "ISC"];

    c.bench_function("spdx_lookup_identifiers", |b| {
        b.iter(|| {
            for identifier in identifiers {
                black_box(licenses.get_license_by_identifier(black_box(identifier)));
                black_box(licenses.is_osi_approved_by_identifier(black_box(identifier)));
            }
        })
    });
}

fn bench_identifier_by_name(c: &mut Criterion) {
    let licenses = SpdxLicenses::new();
    let names = [
        "MIT License",
        "Apache License 2.0",
        "ISC License",
        "GNU General Public License v3.0 only",
    ];

    c.bench_function("spdx_identifier_by_name", |b| {
        b.iter(|| {
            for name in names {
                black_box(licenses.get_identifier_by_name(black_box(name)));
            }
        })
    });
}

criterion_group!(
    benches,
    bench_new,
    bench_validate,
    bench_lookup,
    bench_identifier_by_name
);
criterion_main!(benches);
