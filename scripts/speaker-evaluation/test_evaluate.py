import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import evaluate as e


def turn(a, b, who='A'):
    return dict(start=a, end=b, speaker=who)


def transcript(a=0, b=2):
    return {'segments': [dict(audio_start_time=a, audio_end_time=b, text='Hello café') ]}


def diar(rows):
    return dict(run_id='test-run', audio_duration_seconds=20, diarization=rows)


class EvaluationTests(unittest.TestCase):
    def test_confirmed_single_speaker_and_source_preserved(self):
        source = transcript()
        result = e.align(source, diar([turn(0, 3)]), {'A': 'Matt'}, 0)[0]
        self.assertEqual(result['speaker'], 'Matt')
        for k, v in source['segments'][0].items():
            self.assertEqual(result[k], v)
        self.assertNotIn('speaker', source['segments'][0])

    def test_unconfirmed_cluster_stays_unknown(self):
        self.assertEqual(e.align(transcript(), diar([turn(0, 3)]), {}, 0)[0]['speaker'], 'Unknown')

    def test_overlap_never_assigns_whole_text(self):
        result = e.align(transcript(), diar([turn(0, 2), turn(.5, 1, 'B')]), {'A': 'Matt', 'B': 'Patrick'}, 0)[0]
        self.assertEqual(result['speaker'], 'Unknown')
        self.assertEqual(result['attribution_reason'], 'multiple_speakers')

    def test_sequential_speaker_change_not_majority_vote(self):
        result = e.align(transcript(), diar([turn(0, 1.9), turn(1.9, 2, 'B')]), {'A': 'Matt'}, 0)[0]
        self.assertEqual(result['speaker'], 'Unknown')

    def test_speech_gap_not_silently_named(self):
        result = e.align(transcript(), diar([turn(0, .5), turn(1, 2)]), {'A': 'Matt'}, 0)[0]
        self.assertEqual(result['speaker'], 'Unknown')

    def test_adjacent_same_speaker_spans_merge_logically(self):
        self.assertEqual(e.align(transcript(), diar([turn(0, 1), turn(1, 2)]), {'A': 'Matt'}, 0)[0]['speaker'], 'Matt')

    def test_explicit_offset_and_out_of_bounds(self):
        self.assertEqual(e.align(transcript(), diar([turn(3, 5)]), {'A': 'Matt'}, 3)[0]['speaker'], 'Matt')
        for offset in [-1, 30, float('nan')]:
            with self.assertRaises(ValueError):
                e.align(transcript(), diar([]), {}, offset)

    def test_bad_intervals_fail(self):
        for a, b in [(0, 0), (-1, 2), (2, 1), (float('nan'), 2), (0, float('inf')), (True, 2), ('0', 2)]:
            with self.assertRaises(ValueError):
                e.turns([turn(a, b)])

    def test_name_map_bound_to_run_and_human_confirmation(self):
        valid = dict(run_id='test-run', speakers={'A': {'name': 'Matt', 'confirmed': True}})
        self.assertEqual(e.confirmed_names(valid, 'test-run', {'A'}), {'A': 'Matt'})
        for run, labels in [('other-run', {'A'}), ('test-run', {'B'}), (None, {'A'})]:
            with self.assertRaises(ValueError):
                e.confirmed_names(valid, run, labels)
        valid['speakers']['A']['confirmed'] = False
        with self.assertRaises(ValueError):
            e.confirmed_names(valid, 'test-run', {'A'})

    def test_scoring_distinguishes_wrong_unknown_overlap_false_alarm(self):
        ref = [turn(0, 4, 'Matt'), turn(4, 6, 'Patrick'), turn(5, 6, 'Matt')]
        hyp = [turn(0, 1, 'Matt'), turn(1, 2, 'Patrick'), turn(2, 3, 'Unknown'), turn(6, 7, 'Matt')]
        result = e.score(ref, hyp)
        self.assertEqual(result['correct_seconds'], 1)
        self.assertEqual(result['wrong_name_seconds'], 1)
        self.assertEqual(result['unknown_seconds'], 3)
        self.assertEqual(result['reference_overlap_seconds'], 1)
        self.assertEqual(result['false_alarm_seconds'], 1)
        self.assertEqual(result['wrong_name_rate'], .2)
        self.assertEqual(result['named_coverage'], .4)

    def test_duplicate_intervals_do_not_inflate_seconds(self):
        result = e.score([turn(0, 1), turn(0, 1)], [turn(0, 1)])
        self.assertEqual(result['correct_seconds'], 1)
        self.assertEqual(e.score([], [])['wrong_name_rate'], None)
        with self.assertRaises(ValueError):
            e.score([turn(0, 1, 'Unknown')], [])

    def test_fractional_boundaries(self):
        result = e.score([turn(.1, .3)], [turn(.2, .3)])
        self.assertAlmostEqual(result['correct_seconds'], .1)
        self.assertAlmostEqual(result['unknown_seconds'], .1)

    def test_no_overwrite_and_invalid_json(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / 'result.json'
            e.write_new(p, {'original': True})
            with self.assertRaises(FileExistsError):
                e.write_new(p, {'replacement': True})
            self.assertEqual(e.load(p), {'original': True})
            p.write_text('{"bad": NaN}')
            with self.assertRaises(ValueError):
                e.load(p)

    def test_cli_align_and_score(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d)
            for name, value in [('transcript', transcript()), ('diar', diar([turn(0, 2)])), ('names', dict(run_id='test-run', speakers={'A':dict(name='Matt', confirmed=True)}))]:
                (p / (name + '.json')).write_text(json.dumps(value))
            subprocess.run([sys.executable, e.__file__, 'align', '--transcript', str(p/'transcript.json'), '--diarization', str(p/'diar.json'), '--names', str(p/'names.json'), '--offset-seconds', '0', '--output', str(p/'out.json')], check=True)
            self.assertEqual(e.load(p/'out.json')['segments'][0]['speaker'], 'Matt')
            for name in ['ref', 'hyp']:
                (p/(name+'.json')).write_text(json.dumps([turn(0, 2, 'Matt')]))
            subprocess.run([sys.executable, e.__file__, 'score', '--reference', str(p/'ref.json'), '--hypothesis', str(p/'hyp.json'), '--output', str(p/'score.json')], check=True)
            self.assertEqual(e.load(p/'score.json')['correct_seconds'], 2)

    def test_hosted_or_other_pipeline_rejected(self):
        e.validate_pipeline_config({'pipeline': {'name': 'pyannote.audio.pipelines.SpeakerDiarization'}})
        for name in ['pyannote.audio.pipelines.PyannoteAI', '', 'custom.Module']:
            with self.assertRaises(ValueError):
                e.validate_pipeline_config({'pipeline': {'name': name}})

    def test_missing_model_fails_without_importing_pyannote(self):
        with tempfile.TemporaryDirectory() as d, patch.dict('os.environ', {}, clear=False):
            with self.assertRaises(ValueError):
                e.infer('unused.wav', d)
            import os
            self.assertEqual(os.environ['HF_HUB_OFFLINE'], '1')
            self.assertEqual(os.environ['PYANNOTE_METRICS_ENABLED'], '0')


if __name__ == '__main__':
    unittest.main()
