from datetime import datetime, timedelta, timezone
from dispatch.compute_idle import Sample, idle_average, next_idle_action

NOW = datetime(2026, 11, 25, tzinfo=timezone.utc)


def test_idle_average_refuses_missing_samples_counter_resets_and_uses_leased_cores():
    samples = [Sample(NOW - timedelta(seconds=60-i*10), i*1_000_000) for i in range(7)]
    assert idle_average(samples, NOW, 2, 60, 15) == 5.0
    assert idle_average(samples[1:], NOW, 2, 60, 15) is None
    assert idle_average(samples[:2] + samples[4:], NOW, 2, 60, 15) is None
    assert idle_average(samples[:-1] + [Sample(NOW, 0)], NOW, 2, 60, 15) is None


def test_two_full_answer_windows_and_fitting_queue_are_required_to_reclaim():
    def action(notices, queued=True, activity=NOW-timedelta(hours=3), average=5):
        return next_idle_action(now=NOW, average=average, threshold=10, last_activity=activity,
            notices=notices, idle_seconds=3600, answer_seconds=3600, notice_limit=3,
            expires_at=NOW+timedelta(hours=1), expiry_guard_seconds=600, queued_fit=queued)
    assert action([NOW-timedelta(hours=2), NOW-timedelta(minutes=59)]) is None
    assert action([NOW-timedelta(hours=2), NOW-timedelta(hours=1)]) == 'reclaim'
    assert action([NOW-timedelta(hours=2), NOW-timedelta(hours=1)], queued=False) == 'notice'
    assert action([NOW-timedelta(hours=2), NOW-timedelta(hours=1)], average=None) is None
    assert action([NOW-timedelta(hours=2), NOW-timedelta(hours=1)], activity=NOW-timedelta(minutes=5)) is None
