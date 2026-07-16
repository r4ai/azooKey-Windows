import type { Meta, StoryObj } from "@storybook/react-vite"
import { expect, waitFor } from "storybook/test"
import {
  CandidatePreview,
  type CandidatePreviewDocumentWindow,
} from "./candidate-preview"

const defaultCandidates = [
  "こんにちは",
  "今日は",
  "コンニチハ",
  "こんにちはございます",
  "こんにちわ",
]

const meta = {
  title: "IME/Candidate Window",
  component: CandidatePreview,
  tags: ["autodocs"],
  args: {
    candidates: defaultCandidates,
    selection: 0,
  },
  argTypes: {
    selection: {
      control: { type: "number", min: 0, step: 1 },
    },
    width: {
      control: { type: "number", min: 225, max: 720, step: 1 },
    },
    height: {
      control: { type: "number", min: 100, max: 600, step: 1 },
    },
  },
} satisfies Meta<typeof CandidatePreview>

export default meta
type Story = StoryObj<typeof meta>

function getCandidateIframe(canvasElement: HTMLElement) {
  const iframe = canvasElement.querySelector<HTMLIFrameElement>(
    'iframe[title="変換候補プレビュー"]',
  )
  if (!iframe) {
    throw new Error("候補プレビュー iframe がありません")
  }

  return iframe
}

export const Default: Story = {}

export const LongJapaneseCandidate: Story = {
  args: {
    candidates: [
      "情報処理学会ソフトウェア工学研究会",
      "自然言語処理による日本語入力支援システム",
      "ソフトウェアアーキテクチャ",
    ],
    selection: 1,
  },
}

export const ScrolledSelection: Story = {
  args: {
    candidates: [
      "第一候補",
      "第二候補",
      "第三候補",
      "第四候補",
      "第五候補",
      "第六候補",
      "第七候補",
      "第八候補",
      "第九候補",
      "第十候補",
    ],
    selection: 7,
  },
  play: async ({ canvasElement }) => {
    const iframe = getCandidateIframe(canvasElement)

    await waitFor(() => {
      const candidateList = iframe.contentDocument?.getElementById("candidate-list")
      const selectedCandidate = candidateList?.querySelector("[data-selected]")

      expect(selectedCandidate).toHaveTextContent("第八候補")
      expect(candidateList?.scrollTop).toBeGreaterThan(0)
    })
  },
}

export const VerifiesCandidateState: Story = {
  play: async ({ canvasElement }) => {
    const iframe = getCandidateIframe(canvasElement)

    await waitFor(() => {
      const candidateList = iframe.contentDocument?.getElementById("candidate-list")
      expect(candidateList?.children).toHaveLength(defaultCandidates.length)
    })

    const candidateList = iframe.contentDocument?.getElementById("candidate-list")
    const selectedCandidate = candidateList?.querySelector("[data-selected]")

    await expect(selectedCandidate).toHaveTextContent(defaultCandidates[0])
    await expect(selectedCandidate).toHaveAttribute("title", defaultCandidates[0])
  },
}

export const UpdatesCandidateState: Story = {
  play: async ({ canvasElement }) => {
    const iframe = getCandidateIframe(canvasElement)
    const candidateWindow = iframe.contentWindow as CandidatePreviewDocumentWindow | null

    await waitFor(() => {
      if (!candidateWindow?.updateCandidateState) {
        throw new Error("候補 UI API を初期化中です")
      }
    })
    const updateCandidateState = candidateWindow?.updateCandidateState
    if (!updateCandidateState) {
      throw new Error("候補 UI API を初期化できませんでした")
    }

    updateCandidateState(["更新後の候補", "次の候補"], 1)

    await waitFor(() => {
      const candidateList = iframe.contentDocument?.getElementById("candidate-list")
      const selectedCandidate = candidateList?.querySelector("[data-selected]")

      expect(candidateList?.children).toHaveLength(2)
      expect(selectedCandidate).toHaveTextContent("次の候補")
    })

    updateCandidateState(["更新後の候補"], -1)

    await waitFor(() => {
      const candidateList = iframe.contentDocument?.getElementById("candidate-list")
      expect(candidateList?.querySelector("[data-selected]")).toBeNull()
    })
  },
}
