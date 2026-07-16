import type { Meta, StoryObj } from "@storybook/react-vite"
import { withSettingsFrame } from "@/storybook/decorators"
import { General } from "./general"

const meta = {
  title: "Pages/General",
  component: General,
  tags: ["autodocs"],
  decorators: [withSettingsFrame],
  parameters: {
    route: "/",
  },
} satisfies Meta<typeof General>

export default meta
type Story = StoryObj<typeof meta>

export const Default: Story = {}
