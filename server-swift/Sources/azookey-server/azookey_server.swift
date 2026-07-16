import KanaKanjiConverterModule
import Foundation
import ffi

private let ffiSuccess: Int32 = 0
private let ffiInvalidArgument: Int32 = 1

private struct ServerConfiguration: Equatable {
    var enableZenzai = false
    var profile = ""
    var context = ""
}

private struct CandidatePayload {
    var text: String
    var subtext: String
    var correspondingCount: Int32
}

// Rust serializes every stateful FFI call with MyAzookeyService::ffi_state.
// Initialize runs before the service is published, so these values do not need
// a Swift global actor. Marking a synchronous C entry point @MainActor makes
// Swift trap when Tokio calls it from a worker thread.
nonisolated(unsafe) private var converter: KanaKanjiConverter?
nonisolated(unsafe) private var composingText = ComposingText()
nonisolated(unsafe) private var executableURL: URL?
nonisolated(unsafe) private var configuration = ServerConfiguration()
nonisolated(unsafe) private var zenzaiAllowed = true
nonisolated(unsafe) private var cachedTextReplacer: TextReplacer?
nonisolated(unsafe) private var cachedOptions: ConvertRequestOptions?
nonisolated(unsafe) private var cachedOptionsConfiguration: ServerConfiguration?

private func requestOptions() -> ConvertRequestOptions? {
    guard let executableURL else {
        return nil
    }
    if let cachedOptions, cachedOptionsConfiguration == configuration {
        return cachedOptions
    }

    let zenzaiMode: ConvertRequestOptions.ZenzaiMode
    if zenzaiAllowed && configuration.enableZenzai {
        zenzaiMode = .on(
            weight: executableURL.appendingPathComponent("zenz.gguf"),
            inferenceLimit: 1,
            requestRichCandidates: false,
            personalizationMode: nil,
            versionDependentMode: .v3(
                .init(
                    profile: configuration.profile,
                    leftSideContext: configuration.context
                )
            )
        )
    } else {
        zenzaiMode = .off
    }

    let options = ConvertRequestOptions(
        requireJapanesePrediction: true,
        requireEnglishPrediction: false,
        keyboardLanguage: .ja_JP,
        learningType: .nothing,
        memoryDirectoryURL: URL(filePath: "./test"),
        sharedContainerURL: URL(filePath: "./test"),
        textReplacer: cachedTextReplacer ?? .empty,
        specialCandidateProviders: nil,
        zenzaiMode: zenzaiMode,
        metadata: .init(versionString: "Azookey for Windows")
    )
    cachedOptions = options
    cachedOptionsConfiguration = configuration
    return options
}

private func reloadConfiguration() {
    var updated = ServerConfiguration()
    updated.context = configuration.context

    if let appDataPath = ProcessInfo.processInfo.environment["APPDATA"] {
        let settingsPath = URL(filePath: appDataPath)
            .appendingPathComponent("Azookey/settings.json")
        do {
            let data = try Data(contentsOf: settingsPath)
            if let json = try JSONSerialization.jsonObject(with: data) as? [String: Any],
               let zenzai = json["zenzai"] as? [String: Any] {
                updated.enableZenzai = zenzai["enable"] as? Bool ?? false
                updated.profile = zenzai["profile"] as? String ?? ""
            }
        } catch {
            // Missing settings are equivalent to the defaults. Log malformed/unreadable files
            // once per explicit reload rather than once per key event.
            print("Failed to read settings: \(error)")
        }
    }

    let modelConfigurationChanged =
        updated.enableZenzai != configuration.enableZenzai ||
        updated.profile != configuration.profile
    configuration = updated
    cachedOptions = nil
    cachedOptionsConfiguration = nil
    if modelConfigurationChanged {
        converter?.stopComposition()
    }
}

private func duplicateCString(_ string: String) -> UnsafeMutablePointer<CChar>? {
    string.withCString { pointer in
#if os(Windows)
        _strdup(pointer)
#else
        strdup(pointer)
#endif
    }
}

private func releaseCString(_ pointer: UnsafeMutablePointer<CChar>?) {
    guard let pointer else {
        return
    }
    free(UnsafeMutableRawPointer(pointer))
}

private func constructCandidateString(candidate: Candidate, hiragana: String) -> String {
    var remainingHiragana = hiragana
    var result = ""

    for data in candidate.data {
        if remainingHiragana.count < data.ruby.count {
            result += remainingHiragana
            break
        }
        remainingHiragana.removeFirst(data.ruby.count)
        result += data.word
    }

    return result
}

package func rawInputString(from pieces: [InputPiece]) -> String {
    String(
        pieces.compactMap { piece -> Character? in
            switch piece {
            case .character(let character):
                return character
            case .key(intention: let character, modifiers: _):
                return character
            case .compositionSeparator:
                return nil
            }
        }
    )
}

private func candidatePayloads() -> [CandidatePayload]? {
    guard let converter, let options = requestOptions() else {
        return nil
    }

    let hiragana = composingText.convertTarget
    let converted = converter.requestCandidates(composingText, options: options)
    var seenTexts = Set<String>()
    var result: [CandidatePayload] = []
    result.reserveCapacity(converted.mainResults.count)

    for candidate in converted.mainResults {
        let text = constructCandidateString(candidate: candidate, hiragana: hiragana)
        guard seenTexts.insert(text).inserted else {
            continue
        }

        var afterComposingText = composingText
        afterComposingText.prefixComplete(composingCount: candidate.composingCount)
        let completedSurfaceCount = hiragana.count - afterComposingText.convertTarget.count
        guard completedSurfaceCount >= 0,
              completedSurfaceCount <= hiragana.count,
              let correspondingCount = Int32(exactly: completedSurfaceCount) else {
            return nil
        }

        result.append(
            CandidatePayload(
                text: text,
                subtext: afterComposingText.convertTarget,
                correspondingCount: correspondingCount
            )
        )
    }

    return result
}

private func releaseCandidateList(
    _ list: UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?>,
    initializedCount: Int
) {
    for index in 0..<initializedCount {
        guard let candidate = list[index] else {
            continue
        }
        releaseCString(candidate.pointee.text)
        releaseCString(candidate.pointee.subtext)
        candidate.deinitialize(count: 1)
        candidate.deallocate()
    }
    list.deinitialize(count: initializedCount)
    list.deallocate()
}

private func allocateCandidateList(
    _ payloads: [CandidatePayload]
) -> UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?>? {
    guard !payloads.isEmpty else {
        return nil
    }

    let list = UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?>
        .allocate(capacity: payloads.count)
    var initializedCount = 0

    for payload in payloads {
        guard let text = duplicateCString(payload.text) else {
            releaseCandidateList(list, initializedCount: initializedCount)
            return nil
        }
        guard let subtext = duplicateCString(payload.subtext) else {
            releaseCString(text)
            releaseCandidateList(list, initializedCount: initializedCount)
            return nil
        }

        let candidate = UnsafeMutablePointer<FFICandidate>.allocate(capacity: 1)
        candidate.initialize(
            to: FFICandidate(
                text: text,
                subtext: subtext,
                correspondingCount: payload.correspondingCount
            )
        )
        list.advanced(by: initializedCount).initialize(to: candidate)
        initializedCount += 1
    }

    return list
}

private func copyCurrentComposingText(
    cursorPointer: UnsafeMutablePointer<Int32>
) -> UnsafeMutablePointer<CChar>? {
    guard let cursor = Int32(exactly: composingText.convertTargetCursorPosition) else {
        return nil
    }
    cursorPointer.pointee = cursor
    return duplicateCString(composingText.convertTarget)
}

@_cdecl("LoadConfig")
public func loadConfig() -> Int32 {
    reloadConfiguration()
    return ffiSuccess
}

@_cdecl("Initialize")
public func initialize(
    path: UnsafePointer<CChar>?,
    useZenzai: Int32
) -> Int32 {
    guard let path else {
        return ffiInvalidArgument
    }
    let pathString = String(cString: path)
    guard !pathString.isEmpty else {
        return ffiInvalidArgument
    }

    converter?.stopComposition()
    composingText.stopComposition()

    let newExecutableURL = URL(filePath: pathString)
    executableURL = newExecutableURL
    zenzaiAllowed = useZenzai != 0
    cachedOptions = nil
    cachedOptionsConfiguration = nil

    let emojiURL = newExecutableURL
        .appendingPathComponent("EmojiDictionary")
        .appendingPathComponent("emoji_all_E15.1.txt")
    cachedTextReplacer = TextReplacer { emojiURL }
    converter = KanaKanjiConverter(
        dictionaryURL: newExecutableURL.appendingPathComponent("Dictionary"),
        preloadDictionary: true
    )
    reloadConfiguration()

    // Warm dictionary/model initialization without leaking the warm-up composition into the
    // first real conversion session.
    if let converter, let options = requestOptions() {
        composingText.insertAtCursorPosition("a", inputStyle: .roman2kana)
        _ = converter.requestCandidates(composingText, options: options)
        composingText.stopComposition()
        converter.stopComposition()
    }

    return ffiSuccess
}

@_cdecl("AppendText")
public func appendText(
    input: UnsafePointer<CChar>?,
    cursorPointer: UnsafeMutablePointer<Int32>?
) -> UnsafeMutablePointer<CChar>? {
    guard converter != nil, let input, let cursorPointer else {
        return nil
    }
    composingText.insertAtCursorPosition(String(cString: input), inputStyle: .roman2kana)
    return copyCurrentComposingText(cursorPointer: cursorPointer)
}

@_cdecl("RemoveText")
public func removeText(
    count: Int32,
    cursorPointer: UnsafeMutablePointer<Int32>?
) -> UnsafeMutablePointer<CChar>? {
    guard converter != nil, count > 0, let cursorPointer else {
        return nil
    }
    composingText.deleteBackwardFromCursorPosition(count: Int(count))
    return copyCurrentComposingText(cursorPointer: cursorPointer)
}

@_cdecl("MoveCursor")
public func moveCursor(
    offset: Int32,
    cursorPointer: UnsafeMutablePointer<Int32>?
) -> UnsafeMutablePointer<CChar>? {
    guard converter != nil, let cursorPointer else {
        return nil
    }
    _ = composingText.moveCursorFromCursorPosition(count: Int(offset))
    return copyCurrentComposingText(cursorPointer: cursorPointer)
}

@_cdecl("ShrinkText")
public func shrinkText(offset: Int32) -> UnsafeMutablePointer<CChar>? {
    guard converter != nil,
          offset >= 0,
          Int(offset) <= composingText.convertTarget.count else {
        return nil
    }
    composingText.prefixComplete(composingCount: .surfaceCount(Int(offset)))
    return duplicateCString(composingText.convertTarget)
}

@_cdecl("CommitPrefixAndAppend")
public func commitPrefixAndAppend(
    offset: Int32,
    input: UnsafePointer<CChar>?,
    cursorPointer: UnsafeMutablePointer<Int32>?
) -> UnsafeMutablePointer<CChar>? {
    guard converter != nil,
          offset >= 0,
          Int(offset) <= composingText.convertTarget.count,
          let input,
          let cursorPointer else {
        return nil
    }

    composingText.prefixComplete(composingCount: .surfaceCount(Int(offset)))
    composingText.insertAtCursorPosition(String(cString: input), inputStyle: .roman2kana)
    return copyCurrentComposingText(cursorPointer: cursorPointer)
}

@_cdecl("ClearText")
public func clearText() {
    composingText.stopComposition()
    converter?.stopComposition()
    if !configuration.context.isEmpty {
        configuration.context = ""
        cachedOptions = nil
        cachedOptionsConfiguration = nil
    }
}

@_cdecl("GetComposedText")
public func getComposedText(
    lengthPointer: UnsafeMutablePointer<Int32>?
) -> UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?>? {
    guard let lengthPointer else {
        return nil
    }
    lengthPointer.pointee = -1

    guard let payloads = candidatePayloads(),
          let length = Int32(exactly: payloads.count) else {
        return nil
    }
    guard !payloads.isEmpty else {
        lengthPointer.pointee = 0
        return nil
    }
    guard let list = allocateCandidateList(payloads) else {
        return nil
    }

    lengthPointer.pointee = length
    return list
}

@_cdecl("GetRawInput")
public func getRawInput() -> UnsafeMutablePointer<CChar>? {
    guard converter != nil else {
        return nil
    }
    return duplicateCString(rawInputString(from: composingText.input.map(\.piece)))
}

@_cdecl("FreeCString")
public func freeCString(pointer: UnsafeMutablePointer<CChar>?) {
    releaseCString(pointer)
}

@_cdecl("FreeCandidateList")
public func freeCandidateList(
    candidates: UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?>?,
    length: Int32
) {
    guard let candidates, length >= 0 else {
        return
    }
    releaseCandidateList(candidates, initializedCount: Int(length))
}

@_cdecl("SetContext")
public func setContext(context: UnsafePointer<CChar>?) -> Int32 {
    guard let context else {
        return ffiInvalidArgument
    }
    let contextString = String(cString: context)
    if configuration.context != contextString {
        configuration.context = contextString
        cachedOptions = nil
        cachedOptionsConfiguration = nil
    }
    return ffiSuccess
}
