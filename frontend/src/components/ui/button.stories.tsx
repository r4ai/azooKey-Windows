import type { Meta, StoryObj } from "@storybook/react-vite"
import { expect, fn } from "storybook/test"
import { Button } from "./button"

const meta = {
  title: "UI/Button",
  component: Button,
  tags: ["autodocs"],
  parameters: {
    layout: "centered",
  },
  argTypes: {
    variant: {
      control: "select",
      options: ["default", "secondary", "outline", "ghost", "destructive", "link"],
    },
    size: {
      control: "select",
      options: ["default", "sm", "lg", "icon"],
    },
  },
  args: {
    children: "保存する",
    onClick: fn(),
  },
} satisfies Meta<typeof Button>

export default meta
type Story = StoryObj<typeof meta>

export const Default: Story = {}

export const Secondary: Story = {
  args: {
    children: "キャンセル",
    variant: "secondary",
  },
}

export const Destructive: Story = {
  args: {
    children: "削除する",
    variant: "destructive",
  },
}

export const Disabled: Story = {
  args: {
    disabled: true,
  },
}

export const Clicks: Story = {
  play: async ({ args, canvas, userEvent }) => {
    await userEvent.click(canvas.getByRole("button", { name: "保存する" }))
    await expect(args.onClick).toHaveBeenCalledTimes(1)
  },
}
