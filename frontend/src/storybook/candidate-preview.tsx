import { useCallback, useEffect, useRef } from "react"

export type CandidatePreviewDocumentWindow = Window & {
  azookeyCandidateUiReady?: Promise<void>
  updateCandidateState?: (candidates: string[], selection: number) => void
}

export type CandidatePreviewProps = {
  candidates: string[]
  selection: number
  width?: number
  height?: number
}

export function CandidatePreview({
  candidates,
  selection,
  width = 360,
  height = 260,
}: CandidatePreviewProps) {
  const iframeRef = useRef<HTMLIFrameElement>(null)

  const renderCandidateState = useCallback(() => {
    const candidateWindow = iframeRef.current
      ?.contentWindow as CandidatePreviewDocumentWindow | null
    if (!candidateWindow) {
      return
    }

    const update = () => candidateWindow.updateCandidateState?.(candidates, selection)
    if (candidateWindow.azookeyCandidateUiReady) {
      void candidateWindow.azookeyCandidateUiReady.then(update)
    } else {
      update()
    }
  }, [candidates, selection])

  useEffect(() => {
    renderCandidateState()
  }, [renderCandidateState])

  return (
    <div className="rounded-xl bg-muted p-8 shadow-sm">
      <iframe
        ref={iframeRef}
        title="変換候補プレビュー"
        src="/candidate-ui/candidate.html"
        onLoad={renderCandidateState}
        style={{ width, height }}
        className="block border-0"
      />
    </div>
  )
}
