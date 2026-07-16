import type { Meta, StoryObj } from "@storybook/react-vite"
import { withSettingsFrame } from "@/storybook/decorators"
import { Appearance } from "./appearance"

const meta = {
  title: "Pages/Appearance",
  component: Appearance,
  tags: ["autodocs"],
  decorators: [withSettingsFrame],
  parameters: {
    route: "/appearance",
  },
} satisfies Meta<typeof Appearance>

export default meta
type Story = StoryObj<typeof meta>

export const Default: Story = {}
