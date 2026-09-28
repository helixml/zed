package main

import "testing"

func TestResponseEntryIsolationKey(t *testing.T) {
	previous := map[responseEntryKey]string{{messageID: "3", content: "first"}: "interaction-1"}
	if _, leaked := responseEntryLeak(previous, "3", "second"); leaked {
		t.Fatal("same message ID with different content must not be treated as leakage")
	}
	if owner, leaked := responseEntryLeak(previous, "3", "first"); !leaked || owner != "interaction-1" {
		t.Fatal("exact message ID and content reuse must be treated as leakage")
	}
}

func TestHasIntermediatePhase15Content(t *testing.T) {
	tests := []struct {
		name    string
		lengths []int
		final   int
		want    bool
	}{
		{name: "duplicate snapshots with progress", lengths: []int{0, 0, 4, 4, 1494, 2698, 2698}, final: 2698, want: true},
		{name: "build 4644 provider cadence", lengths: []int{0, 3, 346, 498, 2832}, final: 2832, want: true},
		{name: "only terminal content", lengths: []int{0, 0, 2698, 2698}, final: 2698, want: false},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			samples := make([]phase15AddSample, len(test.lengths))
			for i, length := range test.lengths {
				samples[i].contentLen = length
			}
			if got := hasIntermediatePhase15Content(samples, test.final); got != test.want {
				t.Fatalf("hasIntermediatePhase15Content() = %v, want %v", got, test.want)
			}
		})
	}
}
