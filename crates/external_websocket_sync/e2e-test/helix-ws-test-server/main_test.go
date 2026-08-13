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
