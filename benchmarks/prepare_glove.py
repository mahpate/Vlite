"""Export a bounded GloVe ANN-Benchmarks train/test subset without publishing vectors."""
import argparse
import datetime
import hashlib
import json
from pathlib import Path

SOURCE_SHA256 = "544af1d5e84e112cd4749571dcfd8ca109818a572f850af75a3a09e093a953c4"


def sha256(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def prepare(source, output, records, queries, updates):
    # Import only for this optional data-preparation command, not campaign execution.
    import h5py
    import numpy as np

    source = source.resolve(strict=True)
    source_hash = sha256(source)
    if source_hash != SOURCE_SHA256:
        raise ValueError("GloVe HDF5 SHA-256 differs from the verified campaign source; review dataset provenance before changing the pin")
    with h5py.File(source, "r") as dataset:
        if dataset["train"].shape != (1_183_514, 100) or dataset["test"].shape != (10_000, 100):
            raise ValueError("Expected ANN-Benchmarks glove-100-angular train/test shapes")
        if dataset.attrs.get("distance") != "angular":
            raise ValueError("Expected angular distance metadata")
        selections = [
            ("vectors.txt", "train", 0, records),
            ("queries.txt", "test", 0, queries),
            ("updates.txt", "train", records, records + updates),
        ]
        # Check before creating any output; no filtering or normalization.
        arrays = []
        for name, split, start, stop in selections:
            values = np.asarray(dataset[split][start:stop], dtype="<f4")
            if not np.isfinite(values).all() or not np.any(values != 0, axis=1).all():
                raise ValueError("Nonfinite or zero-norm embedding; no rows are silently removed")
            arrays.append(values)
        output.mkdir(parents=False, exist_ok=False)
        manifest = {
            "schema_version": 1,
            "dataset": "ANN-Benchmarks glove-100-angular",
            "source_url": "https://ann-benchmarks.com/glove-100-angular.hdf5",
            "source_sha256": source_hash,
            "source_bytes": source.stat().st_size,
            "prepared_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "preparation_versions": {"h5py": h5py.__version__, "numpy": np.__version__},
            "model": "GloVe Twitter 27B, 100 dimensions (ANN-Benchmarks conversion)",
            "model_source": "https://nlp.stanford.edu/projects/glove/",
            "conversion_source": "https://github.com/erikbern/ann-benchmarks/blob/main/ann_benchmarks/datasets.py",
            "license": "Open Data Commons Public Domain Dedication and License (PDDL) v1.0",
            "license_url": "https://opendatacommons.org/licenses/pddl/1-0/",
            "license_declaration_url": "https://nlp.stanford.edu/projects/glove/",
            "citation": "Jeffrey Pennington, Richard Socher, Christopher D. Manning. GloVe: Global Vectors for Word Representation. EMNLP 2014.",
            "dimensions": 100,
            "normalization": "none; source values cast to f32 and exported with 9 significant digits",
            "split_policy": "prefixes of upstream train/test splits; updates are disjoint train rows after the base subset; not the full ANN-Benchmarks dataset",
            "upstream_shapes": {"train": list(dataset["train"].shape), "test": list(dataset["test"].shape)},
            "files": [],
        }
        for (name, split, start, stop), values in zip(selections, arrays):
            path = output / name
            with path.open("x", encoding="ascii", newline="\n") as stream:
                for row in values:
                    stream.write(" ".join(format(float(value), ".9g") for value in row) + "\n")
            # Ensure the f32 text round trip is exact.
            decoded = np.loadtxt(path, dtype="<f4", ndmin=2)
            if not np.array_equal(values.view("<u4"), decoded.view("<u4")):
                raise ValueError("f32 text round trip changed source components")
            manifest["files"].append({
                "name": name, "split": split, "start": start, "stop": stop,
                "rows": stop - start, "sha256": sha256(path),
                "decoded_f32_sha256": hashlib.sha256(values.tobytes()).hexdigest(),
            })
    manifest["preparer_sha256"] = sha256(__file__)
    with (output / "dataset-manifest.json").open("x", encoding="utf-8", newline="\n") as stream:
        json.dump(manifest, stream, indent=2)
        stream.write("\n")
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--records", type=int, default=3000)
    parser.add_argument("--queries", type=int, default=100)
    parser.add_argument("--updates", type=int, default=1600)
    args = parser.parse_args()
    if not (10 <= args.records <= 10000 and 1 <= args.queries <= 100
            and 1 <= args.updates <= 25600):
        parser.error("records 10..10000, queries 1..100, updates 1..25600")
    args.source.resolve(strict=True)
    manifest = prepare(args.source, args.output, args.records, args.queries, args.updates)
    print(f"Exported {args.records} base, {args.queries} query, {args.updates} update vectors; source SHA-256 {manifest['source_sha256']}")


if __name__ == "__main__":
    main()
