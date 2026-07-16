import type { Meta, StoryObj } from "@storybook/react-vite"
import { expect } from "storybook/test"
import { withSettingsFrame } from "@/storybook/decorators"
import { setTauriMock } from "@/storybook/tauri"
import { Zenzai } from "./zenzai"

const disabledConfiguration = {
  zenzai: {
    enable: false,
    profile: "",
    backend: "",
  },
}

const meta = {
  title: "Pages/Zenzai",
  component: Zenzai,
  tags: ["autodocs"],
  decorators: [withSettingsFrame],
  parameters: {
    route: "/zenzai",
  },
  loaders: [() => {
    setTauriMock({ config: disabledConfiguration })
    return {}
  }],
} satisfies Meta<typeof Zenzai>

export default meta
type Story = StoryObj<typeof meta>

export const Disabled: Story = {}

export const EnabledWithCpu: Story = {
  loaders: [() => {
    setTauriMock({
      config: {
        zenzai: {
          enable: true,
          profile: "山田太郎。数学科で自然言語処理を研究しています。",
          backend: "cpu",
        },
      },
      capabilities: {
        cpu: true,
        cuda: false,
        vulkan: true,
      },
    })
    return {}
  }],
}

export const LongJapaneseProfile: Story = {
  loaders: [() => {
    setTauriMock({
      config: {
        zenzai: {
          enable: true,
          profile:
            "私は日本語入力支援ソフトウェアを日常的に利用しています。専門用語、固有名詞、技術文書の表記ゆれをできるだけ正確に変換したいと考えています。",
          backend: "vulkan",
        },
      },
      capabilities: {
        cpu: true,
        cuda: true,
        vulkan: true,
      },
    })
    return {}
  }],
}

export const EnablesAndEditsProfile: Story = {
  play: async ({ canvas, userEvent }) => {
    const toggle = canvas.getByRole("switch", { name: "Zenzaiを有効化" })
    const profile = canvas.getByLabelText("変換プロファイル")

    await expect(toggle).not.toBeChecked()
    await expect(profile).toBeDisabled()
    await userEvent.click(toggle)
    await expect(profile).toBeEnabled()
    await userEvent.type(profile, "ソフトウェア開発者です。")
    await expect(profile).toHaveValue("ソフトウェア開発者です。")
  },
}
