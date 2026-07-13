import Testing
@testable import azookey_server

@Test
@MainActor
func ffiRejectsNullRequiredArguments() {
    #expect(initialize(path: nil, useZenzai: 1) == 1)
    #expect(setContext(context: nil) == 1)
    #expect(appendText(input: nil, cursorPointer: nil) == nil)
    #expect(getComposedText(lengthPointer: nil) == nil)
    #expect(getRawInput() == nil)
}

@Test
func ffiFreeFunctionsAcceptNull() {
    freeCString(pointer: nil)
    freeCandidateList(candidates: nil, length: 0)
}

@Test
func rawInputOmitsSeparatorsAndKeepsKeyIntentions() {
    #expect(
        rawInputString(
            from: [
                .character("k"),
                .compositionSeparator,
                .key(intention: "a", modifiers: []),
                .key(intention: nil, modifiers: [.shift]),
            ]
        ) == "ka"
    )
}
