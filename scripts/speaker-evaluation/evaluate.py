#!/usr/bin/env python3
"""Opt-in, offline speaker benchmark. Never modifies capture artifacts."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import time


def load(path):
    return json.loads(Path(path).read_text(), parse_constant=lambda v: (_ for _ in ()).throw(ValueError(v)))


def write_new(path, value):
    # Exclusive creation: no accidental replacement of a source or prior result.
    with Path(path).open('x', encoding='utf-8') as f:
        json.dump(value, f, ensure_ascii=False, indent=2, allow_nan=False)
        f.write('\n')


def digest(path):
    with Path(path).open('rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()


def turns(rows):
    result = []
    for row in rows:
        start, end, speaker = row['start'], row['end'], row['speaker']
        if (isinstance(start, bool) or isinstance(end, bool)
                or not isinstance(start, (int, float)) or not isinstance(end, (int, float))
                or not math.isfinite(start) or not math.isfinite(end)
                or start < 0 or end <= start or not isinstance(speaker, str) or not speaker.strip()):
            raise ValueError('Invalid speaker interval')
        result.append(dict(start=start, end=end, speaker=speaker))
    return sorted(result, key=lambda r: (r['start'], r['end'], r['speaker']))


def confirmed_names(document, run_id, labels):
    if not isinstance(run_id, str) or not run_id.strip() or document.get('run_id') != run_id:
        raise ValueError('Name map belongs to another inference run')
    names = {}
    for label, entry in document.get('speakers', {}).items():
        if label not in labels or entry.get('confirmed') is not True:
            raise ValueError('Unknown or unconfirmed speaker mapping')
        name = entry.get('name')
        if not isinstance(name, str) or not name.strip() or name == 'Unknown':
            raise ValueError('Invalid confirmed name')
        names[label] = name.strip()
    return names


def align(transcript, diarization, names, offset):
    """Only label whole transcript segments covered by one speaker throughout."""
    if not math.isfinite(offset):
        raise ValueError('Nonfinite clock offset')
    speech = turns(diarization['diarization'])  # Keep overlap, NOT exclusive output.
    duration = diarization['audio_duration_seconds']
    if not isinstance(duration, (int, float)) or not math.isfinite(duration) or duration <= 0:
        raise ValueError('Invalid audio duration')
    if any(r['end'] > duration + 0.001 for r in speech):
        raise ValueError('Diarization outside recording')
    out = []
    for row in transcript['segments']:
        start, end = row['audio_start_time'], row['audio_end_time']
        # Reuse strict timing validation, and preserve original text/times.
        turns([dict(start=start, end=end, speaker='validation')])
        if not isinstance(row['text'], str):
            raise ValueError('Invalid transcript text')
        a, b = start + offset, end + offset
        if a < 0 or b > duration + 0.001:
            raise ValueError('Transcript outside audio: check clock alignment')
        relevant = [r for r in speech if r['start'] < b and r['end'] > a]
        labels = {r['speaker'] for r in relevant}
        edges = sorted({a, b, *(max(a, r['start']) for r in relevant), *(min(b, r['end']) for r in relevant)})
        fully_covered = all(any(r['start'] <= x and r['end'] >= y for r in relevant) for x, y in zip(edges, edges[1:]))
        label = next(iter(labels)) if len(labels) == 1 and fully_covered else None
        reason = 'confirmed' if label in names else 'unnamed_speaker' if label else 'multiple_speakers' if len(labels) > 1 else 'incomplete_speech_coverage'
        out.append({**row, 'speaker': names.get(label, 'Unknown'), 'speaker_cluster': label,
                    'speaker_candidates': sorted(labels), 'attribution_reason': reason})
    return out


def score(reference, hypothesis):
    """Exact named-speaker time on non-overlapping reference speech, not DER."""
    ref, hyp = turns(reference), turns(hypothesis)
    if any(r['speaker'] == 'Unknown' for r in ref):
        raise ValueError('Reference must contain verified identities, not Unknown')
    edges = sorted({r[k] for r in ref + hyp for k in ('start', 'end')})
    totals = dict(correct_seconds=0., wrong_name_seconds=0., unknown_seconds=0.,
                  reference_overlap_seconds=0., false_alarm_seconds=0.)
    for a, b in zip(edges, edges[1:]):
        expected = {r['speaker'] for r in ref if r['start'] <= a and r['end'] >= b}
        predicted = {r['speaker'] for r in hyp if r['start'] <= a and r['end'] >= b}
        if len(expected) > 1:
            key = 'reference_overlap_seconds'
        elif not expected:
            if not predicted:
                continue
            key = 'false_alarm_seconds'
        elif not predicted or 'Unknown' in predicted or len(predicted) != 1:
            key = 'unknown_seconds'
        else:
            key = 'correct_seconds' if expected == predicted else 'wrong_name_seconds'
        totals[key] += b - a
    denominator = sum(totals[k] for k in ('correct_seconds', 'wrong_name_seconds', 'unknown_seconds'))
    return {**totals, 'scored_reference_seconds': denominator,
            'wrong_name_rate': totals['wrong_name_seconds'] / denominator if denominator else None,
            'named_coverage': (totals['correct_seconds'] + totals['wrong_name_seconds']) / denominator if denominator else None}


def validate_pipeline_config(config):
    if config.get('pipeline', {}).get('name') != 'pyannote.audio.pipelines.SpeakerDiarization':
        raise ValueError('Only local SpeakerDiarization pipelines are permitted')


def infer(audio, model):
    # No hosted provider, token prompt, implicit download, or telemetry.
    os.environ['HF_HUB_OFFLINE'] = '1'
    os.environ['HF_HUB_DISABLE_TELEMETRY'] = '1'
    os.environ['PYANNOTE_METRICS_ENABLED'] = '0'
    model = Path(model).resolve(strict=True)
    if not model.is_dir() or not (model / 'config.yaml').is_file():
        raise ValueError('Provide a pre-downloaded, trusted Community-1 directory')
    import yaml
    validate_pipeline_config(yaml.safe_load((model / 'config.yaml').read_text()))
    import importlib.metadata
    import resource
    import sys
    import torch
    from pyannote.audio import Pipeline
    from pyannote.audio.core.io import Audio
    audio_hash = digest(audio)
    model_files = {str(p.relative_to(model)): digest(p) for p in sorted(model.rglob('*')) if p.is_file()}
    model_hash = hashlib.sha256(json.dumps(model_files, sort_keys=True).encode()).hexdigest()
    started = time.perf_counter()
    pipeline = Pipeline.from_pretrained(str(model))
    pipeline.to(torch.device('cpu'))
    output = pipeline(str(Path(audio).resolve(strict=True)))
    elapsed = time.perf_counter() - started
    duration = Audio().get_duration(str(audio))
    result = output.serialize()
    result['diarization'] = turns(result['diarization'])
    result['exclusive_diarization'] = turns(result['exclusive_diarization'])
    import uuid
    return {**result, 'run_id': str(uuid.uuid4()), 'audio_sha256': audio_hash,
            'model_sha256': model_hash, 'model_files': model_files,
            'pyannote_version': importlib.metadata.version('pyannote.audio'),
            'device': 'cpu', 'audio_duration_seconds': duration, 'elapsed_seconds': elapsed,
            'real_time_factor': elapsed / duration if duration else None,
            'process_peak_rss_bytes': resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * (1 if sys.platform == 'darwin' else 1024)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('infer')
    p.add_argument('--audio', required=True)
    p.add_argument('--model-dir', required=True)
    p.add_argument('--output', required=True)
    p = sub.add_parser('align')
    p.add_argument('--transcript', required=True)
    p.add_argument('--diarization', required=True)
    p.add_argument('--names', required=True)
    p.add_argument('--offset-seconds', required=True, type=float)
    p.add_argument('--output', required=True)
    p = sub.add_parser('score')
    p.add_argument('--reference', required=True)
    p.add_argument('--hypothesis', required=True)
    p.add_argument('--output', required=True)
    args = parser.parse_args()
    if Path(args.output).exists():
        parser.error('Output already exists; choose a new derivative file')
    if args.command == 'infer':
        result = infer(args.audio, args.model_dir)
    elif args.command == 'align':
        diar = load(args.diarization)
        labels = {r['speaker'] for r in turns(diar['diarization'])}
        names = confirmed_names(load(args.names), diar['run_id'], labels)
        result = dict(run_id=diar['run_id'], transcript_sha256=digest(args.transcript),
                      diarization_sha256=digest(args.diarization), names_sha256=digest(args.names),
                      offset_seconds=args.offset_seconds,
                      segments=align(load(args.transcript), diar, names, args.offset_seconds))
    else:
        result = score(load(args.reference), load(args.hypothesis))
        result.update(reference_sha256=digest(args.reference), hypothesis_sha256=digest(args.hypothesis))
    write_new(args.output, result)


if __name__ == '__main__':
    main()
