import Foundation
import KanaKanjiConverterModule
import Testing
import azookey_server

@Suite(.serialized)
struct AzookeyServerTests {
    @Test
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

    @Test
    func packagedDictionaryProducesKanjiCandidates() {
        let packageRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        let dictionaryURL = packageRoot
            .appendingPathComponent("azooKey_dictionary_storage", isDirectory: true)
            .appendingPathComponent("Dictionary", isDirectory: true)
        let converter = KanaKanjiConverter(
            dictionaryURL: dictionaryURL,
            preloadDictionary: false
        )
        defer {
            converter.stopComposition()
        }

        var composingText = ComposingText()
        composingText.insertAtCursorPosition("nihongo", inputStyle: .roman2kana)
        let options = ConvertRequestOptions(
            requireJapanesePrediction: true,
            requireEnglishPrediction: false,
            keyboardLanguage: .ja_JP,
            learningType: .nothing,
            memoryDirectoryURL: packageRoot,
            sharedContainerURL: packageRoot,
            textReplacer: .empty,
            specialCandidateProviders: nil,
            zenzaiMode: .off,
            metadata: nil
        )
        let candidateTexts = converter
            .requestCandidates(composingText, options: options)
            .mainResults
            .map(\.text)
        #expect(candidateTexts.contains("日本語"))
    }
}
