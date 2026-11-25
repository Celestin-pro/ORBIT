"""Petite fenêtre affichée sur le PC fixe pendant un appairage.
Lancée par l'agent : pythonw pin_popup.py <PIN> <durée_s>"""
import sys
import tkinter as tk


def main():
    pin = sys.argv[1]
    seconds = int(sys.argv[2]) if len(sys.argv) > 2 else 120

    root = tk.Tk()
    root.title("Cloud gaming – appairage")
    root.configure(bg="#0B0E14")
    root.attributes("-topmost", True)
    root.resizable(False, False)

    tk.Label(root, text="Un appareil demande à se connecter à ce PC",
             fg="#C3CAD8", bg="#0B0E14", font=("Segoe UI", 12)).pack(padx=40, pady=(28, 6))
    tk.Label(root, text="Entre ce code sur le PC portable :",
             fg="#9AA4B8", bg="#0B0E14", font=("Segoe UI", 10)).pack()
    tk.Label(root, text=f"{pin[:3]} {pin[3:]}", fg="#4CC9F0", bg="#0B0E14",
             font=("Consolas", 40, "bold")).pack(pady=14)
    remaining = tk.Label(root, fg="#9AA4B8", bg="#0B0E14", font=("Segoe UI", 9))
    remaining.pack(pady=(0, 8))
    tk.Button(root, text="Refuser", command=root.destroy, relief="flat",
              bg="#252C3B", fg="#E8ECF4", padx=16, pady=6).pack(pady=(0, 24))

    def tick(left):
        if left <= 0:
            root.destroy()
            return
        remaining.config(text=f"Expire dans {left} s · si tu n'as rien demandé, ferme cette fenêtre")
        root.after(1000, tick, left - 1)

    tick(seconds)
    root.update_idletasks()
    w, h = root.winfo_width(), root.winfo_height()
    root.geometry(f"+{(root.winfo_screenwidth() - w) // 2}+{(root.winfo_screenheight() - h) // 2}")
    root.mainloop()


if __name__ == "__main__":
    main()
